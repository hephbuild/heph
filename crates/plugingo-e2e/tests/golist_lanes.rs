#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]
//! `_golist` sees a package's non-Go files content-blind: a names-only lane for
//! most of them, and, for the C/asm-family files of the package directory, the
//! real files staged unhashed beside a header target that hashes only what
//! `go/build` reads (`crates/plugin-go/src/plugingo/nongo_lanes.rs`).
//!
//! Every test here builds, mutates the workspace, rebuilds with a **fresh
//! engine** over the same root (as a second `heph` invocation would), and asserts
//! from the engine's events whether `_golist` — and the targets around it — was a
//! local cache hit or executed ([`Events::outcome`]).
//!
//! The differentials at the bottom hold the header extractor to the real
//! `go/build` instead of to its own reading of `readComments`.

mod common;

use anyhow::Context as _;
use common::{HERMETIC_GO, artifact_bytes, make_workspace, require_go};
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{EResult, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use htestkit::Workspace;
use plugin_go::plugingo::{self, GoPackage};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// D8: hit / executed, from engine events
// ---------------------------------------------------------------------------

/// What one run did with a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Served from the local cache: a `LocalCacheHit`, no `ExecuteStart`.
    Hit,
    /// The driver ran: an `ExecuteStart`.
    Executed,
}

/// The events one request emitted.
struct Events(Vec<BuildEvent>);

impl Events {
    /// Every addr of package `pkg` named `name` that the run touched, with what
    /// it did. Matched on package and name rather than on a full addr: the
    /// variant args `_golist` carries are the provider's business, and a test
    /// that spelled them would break on any change to them.
    fn outcomes(&self, pkg: &str, name: &str) -> BTreeMap<String, Outcome> {
        let mut out = BTreeMap::new();
        for e in &self.0 {
            let (addr, o) = match &e.kind {
                BuildEventKind::ExecuteStart { addr, .. } => (addr, Outcome::Executed),
                BuildEventKind::LocalCacheHit { addr } => (addr, Outcome::Hit),
                _ => continue,
            };
            let Ok(a) = parse_addr(addr) else { continue };
            if a.package.as_str() != pkg || a.name != name {
                continue;
            }
            let slot = out.entry(addr.clone()).or_insert(o);
            // An execute wins over a hit: a hit followed by a re-run (an
            // unusable entry) is still a run.
            if o == Outcome::Executed {
                *slot = Outcome::Executed;
            }
        }
        out
    }

    /// The one outcome of `//<pkg>:<name>` in this run. Panics, listing what the
    /// run touched, if the target was not reached or several of its addrs
    /// disagree — either means the assertion would be about the wrong thing.
    #[track_caller]
    fn outcome(&self, pkg: &str, name: &str) -> Outcome {
        let all = self.outcomes(pkg, name);
        let mut kinds: Vec<Outcome> = all.values().copied().collect();
        kinds.dedup();
        match kinds.as_slice() {
            [one] => *one,
            _ => panic!(
                "//{pkg}:{name}: expected one outcome, got {all:?}\ntouched: {:#?}",
                self.touched()
            ),
        }
    }

    #[track_caller]
    fn assert_hit(&self, pkg: &str, name: &str, why: &str) {
        assert_eq!(
            self.outcome(pkg, name),
            Outcome::Hit,
            "//{pkg}:{name} must be a cache hit: {why}"
        );
    }

    #[track_caller]
    fn assert_executed(&self, pkg: &str, name: &str, why: &str) {
        assert_eq!(
            self.outcome(pkg, name),
            Outcome::Executed,
            "//{pkg}:{name} must re-run: {why}"
        );
    }

    /// `addr: outcome` for everything the run executed or hit, for a failure
    /// message.
    fn touched(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|e| match &e.kind {
                BuildEventKind::ExecuteStart { addr, .. } => Some(format!("exec {addr}")),
                BuildEventKind::LocalCacheHit { addr } => Some(format!("hit  {addr}")),
                _ => None,
            })
            .collect()
    }
}

/// Resolve `addr` in a request that records its events.
///
/// Waits for the request's background work before returning, as `heph run`
/// does on exit: the next run is a new engine, and must find the cache this one
/// wrote.
async fn run(ws: &Workspace, addr: &str) -> anyhow::Result<(Arc<EResult>, Events)> {
    let parsed = parse_addr(addr).with_context(|| format!("parse {addr}"))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = ws.engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(&ws.engine)
        .result_addr(
            rs.clone(),
            &parsed,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let res = res.map_err(|err| {
        // The root cause lives in the failure registry; `result_addr` returns a
        // marker (see `htestkit::Workspace::run_addr`).
        let failures = rs.take_failures();
        if failures.is_empty() {
            return err;
        }
        let msg: Vec<String> = failures
            .iter()
            .map(|f| format!("{}: {:#}", f.addr.format(), f.source))
            .collect();
        anyhow::anyhow!("{}", msg.join("\n"))
    });
    let bg = rs.bg_pending();
    // Dropped before the wait: some background work is only submitted when the
    // request goes (see `remote_cache.rs`'s `run_drain_events`).
    drop(rs);
    let deadline = Instant::now() + Duration::from_secs(30);
    while bg.load(Ordering::Acquire) > 0 {
        anyhow::ensure!(Instant::now() < deadline, "background work never drained");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    Ok((res?, Events(events)))
}

/// A new engine over `ws`'s root and on-disk cache: the next `heph` invocation.
fn reopen(ws: Workspace) -> anyhow::Result<Workspace> {
    let dir = ws.dir;
    drop(ws.engine);
    make_workspace(dir)
}

/// `//<pkg>:_golist@v=host`: the package `go list` reported, and the run's
/// events.
async fn golist(ws: &Workspace, pkg: &str) -> anyhow::Result<(GoPackage, Events)> {
    let (res, ev) = run(ws, &format!("//{pkg}:_golist@v=host")).await?;
    Ok((package_bin(&res)?, ev))
}

/// Decode the `package.bin` among a `_golist` result's artifacts.
fn package_bin(res: &EResult) -> anyhow::Result<GoPackage> {
    use heph::hartifactcontent::WalkEntryKind;
    for artifact in &res.artifacts {
        for entry in artifact.walk().context("walk _golist artifact")? {
            let entry = entry.context("_golist artifact entry")?;
            if !entry.path.ends_with("package.bin") {
                continue;
            }
            if let WalkEntryKind::File { data, .. } = entry.kind {
                return plugingo::decode_go_package(data);
            }
        }
    }
    anyhow::bail!("no package.bin in the _golist result")
}

/// The header target's tar for `pkg`.
async fn headers_tar(ws: &Workspace, pkg: &str) -> anyhow::Result<Vec<u8>> {
    let addr = if pkg.is_empty() {
        "//@heph/go/headers:headers".to_string()
    } else {
        format!("//@heph/go/headers/{pkg}:headers")
    };
    let (res, _) = run(ws, &addr).await?;
    Ok(artifact_bytes(&res))
}

/// `go list`'s package error, `""` for none.
fn pkg_error(p: &GoPackage) -> &str {
    p.error.as_ref().map_or("", |e| e.err.as_str())
}

/// The package's `CFiles` as `go list` reports them with cgo on.
///
/// `_golist` cannot show them: it does not select `CFiles`, and it lists with
/// `CGO_ENABLED=0` outside a race build, where `cmd/go` drops every C file from
/// the package (`load.Package`: "C files are only used with cgo"). That build is
/// still keyed on the headers — the header target is variant-free — so a test
/// asserts the re-run through heph and the semantic change through this.
fn cgo_c_files(root: &Path, pkg: &str) -> anyhow::Result<Vec<String>> {
    let out = go_cmd(None)
        .args(["list", "-e", "-json=CFiles", &format!("./{pkg}")])
        .env("CGO_ENABLED", "1")
        .current_dir(root)
        .output()
        .context("go list CFiles")?;
    anyhow::ensure!(out.status.success(), "go list CFiles: {out:?}");
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('"') && !l.contains(':'))
        .map(|l| l.trim_end_matches(',').trim_matches('"').to_string())
        .collect())
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A temp workspace holding `files` (`path`, `contents`).
fn tree(files: &[(&str, &str)]) -> anyhow::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir().context("tempdir")?;
    for (path, contents) in files {
        write(dir.path(), path, contents)?;
    }
    Ok(dir)
}

fn write(root: &Path, rel: &str, contents: &str) -> anyhow::Result<()> {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(&p, contents).with_context(|| format!("write {}", p.display()))
}

fn remove(root: &Path, rel: &str) -> anyhow::Result<()> {
    let p = root.join(rel);
    std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))
}

fn rename(root: &Path, from: &str, to: &str) -> anyhow::Result<()> {
    std::fs::rename(root.join(from), root.join(to)).with_context(|| format!("rename {from} {to}"))
}

/// Write a linked Go binary out of `result` and run it; its stdout.
fn exec_binary(result: &EResult) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().context("tempdir")?;
    let bin = dir.path().join("bin");
    std::fs::write(&bin, artifact_bytes(result)).context("write binary")?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).context("chmod")?;
    let out = std::process::Command::new(&bin)
        .output()
        .context("run binary")?;
    anyhow::ensure!(out.status.success(), "binary failed: {out:?}");
    String::from_utf8(out.stdout).context("binary stdout is utf8")
}

const GO_MOD: &str = "module example.com/lanes\n\ngo 1.21\n";

/// A `main` package embedding `assets/*.txt` and printing each file it embeds.
const EMBED_MAIN: &str = r#"package main

import (
	"embed"
	"fmt"
	"io/fs"
)

//go:embed assets/*.txt
var assets embed.FS

func main() {
	_ = fs.WalkDir(assets, ".", func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		b, _ := assets.ReadFile(p)
		fmt.Printf("%s=%s\n", p, b)
		return nil
	})
}
"#;

// ---------------------------------------------------------------------------
// T6, T7, T8: assets reach `_golist` by name only
// ---------------------------------------------------------------------------

/// T6: editing an embedded asset leaves `_golist` a hit, re-runs the compile
/// (which stages the asset content-hashed), and the binary carries the new bytes.
#[tokio::test]
async fn t6_editing_an_embedded_asset_keeps_golist_and_rebuilds() -> anyhow::Result<()> {
    require_go!();
    let dir = tree(&[
        ("go.mod", GO_MOD),
        ("main.go", EMBED_MAIN),
        ("assets/a.txt", "one"),
    ])?;
    let ws = make_workspace(dir)?;
    let (res, _) = run(&ws, "//:build@v=host").await?;
    assert_eq!(exec_binary(&res)?, "assets/a.txt=one\n");

    write(ws.dir.path(), "assets/a.txt", "two")?;
    let ws = reopen(ws)?;
    let (res, ev) = run(&ws, "//:build@v=host").await?;
    ev.assert_hit("", "_golist", "only an embedded file's bytes changed");
    ev.assert_executed("", "build_lib", "the compile embeds the bytes");
    assert_eq!(exec_binary(&res)?, "assets/a.txt=two\n");
    Ok(())
}

/// T6b (I5): a package-dir `.h` is a header-read file, so `_golist` stages it
/// unhashed and keys on its header only. Embedding it must still give the
/// compile its real, current bytes: editing its body below the header leaves
/// `_golist` a hit, and the binary prints the new body.
#[tokio::test]
async fn t6b_an_embedded_header_read_file_reaches_the_compile_whole() -> anyhow::Result<()> {
    require_go!();
    const MAIN: &str = r#"package main

import (
	_ "embed"
	"os"
)

//go:embed table.h
var table string

func main() { os.Stdout.WriteString(table) }
"#;
    let dir = tree(&[
        ("go.mod", GO_MOD),
        ("main.go", MAIN),
        ("table.h", "// a table\n\nint one = 1;\n"),
    ])?;
    let ws = make_workspace(dir)?;
    let (res, _) = run(&ws, "//:build@v=host").await?;
    assert_eq!(exec_binary(&res)?, "// a table\n\nint one = 1;\n");

    write(ws.dir.path(), "table.h", "// a table\n\nint two = 2;\n")?;
    let ws = reopen(ws)?;
    let (res, ev) = run(&ws, "//:build@v=host").await?;
    ev.assert_hit("", "_golist", "the header of table.h did not change");
    ev.assert_executed("", "build_lib", "the compile embeds table.h's bytes");
    assert_eq!(
        exec_binary(&res)?,
        "// a table\n\nint two = 2;\n",
        "the binary must embed the edited body, not the header extract"
    );
    Ok(())
}

/// T7: a README, a BUILD file and a testdata file anywhere in the root module
/// reach the root package's `_golist` by name only — editing all three is a hit.
#[tokio::test]
async fn t7_editing_non_go_files_of_the_root_module_keeps_golist() -> anyhow::Result<()> {
    require_go!();
    let dir = tree(&[
        ("go.mod", GO_MOD),
        ("lanes.go", "package lanes\n"),
        ("README.md", "# lanes\n"),
        ("BUILD", "# nothing here\n"),
        ("testdata/case.txt", "input one\n"),
        ("docs/notes.md", "notes\n"),
    ])?;
    let ws = make_workspace(dir)?;
    let (_, ev) = golist(&ws, "").await?;
    ev.assert_executed("", "_golist", "first run");

    let root = ws.dir.path();
    write(root, "README.md", "# lanes, edited\n")?;
    write(root, "BUILD", "# still nothing here\n")?;
    write(root, "testdata/case.txt", "input two\n")?;
    write(root, "docs/notes.md", "more notes\n")?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "").await?;
    ev.assert_hit("", "_golist", "only the bytes of non-Go files changed");
    Ok(())
}

/// T8: a new file matching a `//go:embed` pattern changes what `go list`
/// reports, so the names lane must move `_golist`'s key.
#[tokio::test]
async fn t8_adding_a_file_an_embed_pattern_matches_relists() -> anyhow::Result<()> {
    require_go!();
    let dir = tree(&[
        ("go.mod", GO_MOD),
        ("main.go", EMBED_MAIN),
        ("assets/a.txt", "one"),
    ])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "").await?;
    assert_eq!(pkg.embed_files, vec!["assets/a.txt"]);

    write(ws.dir.path(), "assets/b.txt", "two")?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "").await?;
    ev.assert_executed("", "_golist", "a file was added under an embed pattern");
    assert_eq!(pkg.embed_files, vec!["assets/a.txt", "assets/b.txt"]);
    Ok(())
}

// ---------------------------------------------------------------------------
// T9, T9b–d, B3, T10: the header-read files of the package directory
// ---------------------------------------------------------------------------

/// A Go package `asm` whose directory holds the files given beside `asm.go`.
fn asm_pkg(files: &[(&str, &str)]) -> anyhow::Result<tempfile::TempDir> {
    let mut all = vec![("go.mod", GO_MOD), ("asm/asm.go", "package asm\n")];
    all.extend_from_slice(files);
    tree(&all)
}

const IGNORED_S: &str =
    "//go:build ignore\n\n#include \"textflag.h\"\n\nTEXT ·f(SB),NOSPLIT,$0\n\tRET\n";
const IGNORED_C: &str = "//go:build ignore\n\nint y(void) { return 1; }\n";

/// T9: dropping `//go:build ignore` from a `.s` and then from a `.c` of the
/// package directory each re-list, and `SFiles` / the C files change.
#[tokio::test]
async fn t9_flipping_a_build_constraint_relists() -> anyhow::Result<()> {
    require_go!();
    let dir = asm_pkg(&[("asm/x.s", IGNORED_S), ("asm/y.c", IGNORED_C)])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "asm").await?;
    assert!(pkg.s_files.is_empty(), "{:?}", pkg.s_files);
    assert!(cgo_c_files(ws.dir.path(), "asm")?.is_empty());

    let root = ws.dir.path();
    write(
        root,
        "asm/x.s",
        IGNORED_S.replace("//go:build ignore\n", "").as_str(),
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "x.s lost its ignore constraint");
    assert_eq!(pkg.s_files, vec!["x.s"]);

    let root = ws.dir.path();
    write(
        root,
        "asm/y.c",
        IGNORED_C.replace("//go:build ignore\n", "").as_str(),
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "y.c lost its ignore constraint");
    assert_eq!(cgo_c_files(ws.dir.path(), "asm")?, vec!["y.c"]);
    Ok(())
}

/// T9b: editing a `.c` and a `.s` below their headers re-runs the header
/// target, which produces the same tar, so `_golist` is a hit.
#[tokio::test]
async fn t9b_editing_a_body_below_its_header_keeps_golist() -> anyhow::Result<()> {
    require_go!();
    let dir = asm_pkg(&[("asm/x.s", IGNORED_S), ("asm/y.c", IGNORED_C)])?;
    let ws = make_workspace(dir)?;
    golist(&ws, "asm").await?;
    let before = headers_tar(&ws, "asm").await?;

    let root = ws.dir.path();
    write(
        root,
        "asm/x.s",
        &format!("{IGNORED_S}\nTEXT ·g(SB),NOSPLIT,$0\n\tRET\n"),
    )?;
    write(root, "asm/y.c", &IGNORED_C.replace("return 1", "return 2"))?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("@heph/go/headers/asm", "headers", "its input bytes changed");
    ev.assert_hit("asm", "_golist", "no header changed");
    assert_eq!(
        headers_tar(&ws, "asm").await?,
        before,
        "a body edit must leave the header tar byte-identical"
    );
    Ok(())
}

/// T9c: renaming `y.c` to `y_windows.c`, and then `x.s` to `x_windows.s`, same
/// bytes, each takes the file out of a non-windows listing. The header tar
/// names each file, so the key moves. The `.s` half reads `SFiles` from
/// `_golist`'s own output.
#[tokio::test]
async fn t9c_renaming_into_another_goos_relists() -> anyhow::Result<()> {
    require_go!();
    let body = "int y(void) { return 1; }\n";
    let s = "TEXT ·f(SB),$0\n\tRET\n";
    let dir = asm_pkg(&[("asm/y.c", body), ("asm/x.s", s)])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "asm").await?;
    assert_eq!(cgo_c_files(ws.dir.path(), "asm")?, vec!["y.c"]);
    assert_eq!(pkg.s_files, vec!["x.s"]);

    rename(ws.dir.path(), "asm/y.c", "asm/y_windows.c")?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "a header-read file was renamed");
    assert!(
        cgo_c_files(ws.dir.path(), "asm")?.is_empty(),
        "y_windows.c is not built on this host"
    );

    rename(ws.dir.path(), "asm/x.s", "asm/x_windows.s")?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "a header-read .s file was renamed");
    assert!(
        pkg.s_files.is_empty(),
        "x_windows.s is not built on this host: {:?}",
        pkg.s_files
    );
    Ok(())
}

/// T9d and B3: the first `.c` added to a package, and the last one removed, each
/// re-list — including when the header tar goes from or to holding nothing.
#[tokio::test]
async fn t9d_adding_the_first_and_removing_the_last_c_file_relists() -> anyhow::Result<()> {
    require_go!();
    let dir = asm_pkg(&[])?;
    let ws = make_workspace(dir)?;
    golist(&ws, "asm").await?;

    write(ws.dir.path(), "asm/z.c", "int z;\n")?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "the package's first .c was added");
    assert_eq!(cgo_c_files(ws.dir.path(), "asm")?, vec!["z.c"]);

    remove(ws.dir.path(), "asm/z.c")?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "the package's last .c was removed");
    Ok(())
}

/// B3: removing one of two `.s` files whose headers are identical — the header
/// tar keeps one entry per file, so the key still moves.
#[tokio::test]
async fn b3_removing_one_of_two_identical_header_files_relists() -> anyhow::Result<()> {
    require_go!();
    let s = "// shared header\n\nTEXT ·f(SB),$0\n\tRET\n";
    let dir = asm_pkg(&[("asm/a.s", s), ("asm/b.s", s)])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "asm").await?;
    assert_eq!(pkg.s_files, vec!["a.s", "b.s"]);

    remove(ws.dir.path(), "asm/b.s")?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "asm").await?;
    ev.assert_executed("asm", "_golist", "one of two identical-header files went");
    assert_eq!(pkg.s_files, vec!["a.s"]);
    Ok(())
}

/// T10: a `.c` in a subdirectory belongs to no package's header lane (`go/build`
/// reads only the package directory), so editing it — header included — is a
/// hit.
#[tokio::test]
async fn t10_editing_a_c_file_in_a_subdirectory_keeps_golist() -> anyhow::Result<()> {
    require_go!();
    let dir = asm_pkg(&[("asm/csrc/q.c", "int q;\n")])?;
    let ws = make_workspace(dir)?;
    golist(&ws, "asm").await?;

    write(
        ws.dir.path(),
        "asm/csrc/q.c",
        "//go:build ignore\n\nint q2;\n",
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "asm").await?;
    ev.assert_hit("asm", "_golist", "csrc/q.c is not in the package directory");
    Ok(())
}

// ---------------------------------------------------------------------------
// T15 (I8, B5): a vendored module keeps the content lane
// ---------------------------------------------------------------------------

/// A module vendoring a dependency nothing imports: `vendor/modules.txt` makes
/// Go list it in vendor mode (from `go 1.14`), and heph keep its content lane.
fn vendored_module(prefix: &str, module: &str, go: &str) -> Vec<(String, String)> {
    let p = |rel: &str| format!("{prefix}{rel}");
    vec![
        (p("go.mod"), format!("module {module}\n\ngo {go}\n")),
        (p("vendor/modules.txt"), String::new()),
        (p("vendor/example.com/dep/LICENSE"), "MIT\n".to_string()),
        (p("lib.go"), "package lib\n".to_string()),
        (p("README.md"), "# vendored\n".to_string()),
        (p("sub/sub.go"), "package sub\n".to_string()),
        (p("sub/notes.txt"), "notes\n".to_string()),
    ]
}

fn tree_owned(files: &[(String, String)]) -> anyhow::Result<tempfile::TempDir> {
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    tree(&borrowed)
}

/// T15, root vendor dir, on either side of `go 1.14` (where Go starts turning
/// on `-mod=vendor` by itself): the module's packages list and compile, and
/// editing a vendored file or a non-vendor asset re-lists — the content lane,
/// for the root package and a nested one alike.
#[tokio::test]
async fn t15_a_vendored_module_keeps_the_content_lane() -> anyhow::Result<()> {
    require_go!();
    for go in ["1.13", "1.21"] {
        let dir = tree_owned(&vendored_module("", "example.com/vend", go))?;
        let ws = make_workspace(dir)?;
        let (pkg, _) = golist(&ws, "")
            .await
            .with_context(|| format!("go {go}: list the vendored root package"))?;
        assert_eq!(pkg_error(&pkg), "", "go {go}");
        run(&ws, "//:build_lib@v=host")
            .await
            .with_context(|| format!("go {go}: compile the vendored root package"))?;
        golist(&ws, "sub").await?;

        write(ws.dir.path(), "vendor/example.com/dep/LICENSE", "BSD\n")?;
        let ws = reopen(ws)?;
        let (_, ev) = golist(&ws, "").await?;
        ev.assert_executed("", "_golist", &format!("go {go}: a vendored file changed"));

        write(ws.dir.path(), "README.md", "# vendored, edited\n")?;
        let ws = reopen(ws)?;
        let (_, ev) = golist(&ws, "").await?;
        ev.assert_executed(
            "",
            "_golist",
            &format!("go {go}: content lane, root package"),
        );

        write(ws.dir.path(), "sub/notes.txt", "more notes\n")?;
        let ws = reopen(ws)?;
        let (_, ev) = golist(&ws, "sub").await?;
        ev.assert_executed(
            "sub",
            "_golist",
            &format!("go {go}: content lane, nested package"),
        );
    }
    Ok(())
}

/// T15, nested vendor dir: a vendored module nested inside an unvendored one.
/// The check is per module root, so only the nested module's packages fall back
/// to the content lane; the outer module keeps the names lane.
#[tokio::test]
async fn t15_a_nested_vendored_module_falls_back_alone() -> anyhow::Result<()> {
    require_go!();
    let mut files = vec![
        ("go.mod".to_string(), GO_MOD.to_string()),
        ("lanes.go".to_string(), "package lanes\n".to_string()),
        ("README.md".to_string(), "# outer\n".to_string()),
    ];
    files.extend(vendored_module("inner/", "example.com/inner", "1.21"));
    // The harness declares the `host` variant for the root module only; a
    // variant is looked up within the package's own module.
    files.push((
        "inner/BUILD".to_string(),
        format!(
            "provider_state(provider = \"go\", variants = {{\"host\": {{\"goos\": \"{}\", \"goarch\": \"{}\"}}}})\n",
            hcore::htplatform::os(),
            hcore::htplatform::arch()
        ),
    ));
    let dir = tree_owned(&files)?;
    let ws = make_workspace(dir)?;
    golist(&ws, "").await?;
    let (pkg, _) = golist(&ws, "inner").await?;
    assert_eq!(pkg_error(&pkg), "");

    write(ws.dir.path(), "inner/README.md", "# inner, edited\n")?;
    write(ws.dir.path(), "README.md", "# outer, edited\n")?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "").await?;
    ev.assert_hit("", "_golist", "the outer module is not vendored");
    let (_, ev) = golist(&ws, "inner").await?;
    ev.assert_executed("inner", "_golist", "the inner module is vendored");
    Ok(())
}

// ---------------------------------------------------------------------------
// T13b, T19: the header rules, held to the real `go/build` and `go list`
// ---------------------------------------------------------------------------

/// The Go toolchains the differentials run under: the host `go`, and the pinned
/// default ([`HERMETIC_GO`]) through `GOTOOLCHAIN` — in CI the two are the same
/// release (devenv pins its Go to it), locally they need not be.
fn go_toolchains() -> Vec<Option<String>> {
    let host = std::process::Command::new("go")
        .args(["env", "GOVERSION"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let pinned = format!("go{HERMETIC_GO}");
    if host.as_deref() == Some(pinned.as_str()) {
        vec![None]
    } else {
        vec![None, Some(pinned)]
    }
}

/// A `go` command with a clean environment: no `GOFLAGS` / workspace from the
/// caller, and `GOTOOLCHAIN` set when `toolchain` names one.
fn go_cmd(toolchain: Option<&str>) -> std::process::Command {
    let mut cmd = std::process::Command::new("go");
    cmd.env_remove("GOFLAGS")
        .env("GOWORK", "off")
        .env("GOTOOLCHAIN", toolchain.unwrap_or("local"));
    cmd
}

/// The probe: for each file of a directory, the bytes `go/build` reads of it
/// (through a reader that returns one byte per call, so `bufio` pulls nothing
/// ahead) and `MatchFile`'s verdict.
const PROBE: &str = r#"package main

import (
	"fmt"
	"go/build"
	"io"
	"os"
	"strings"
)

type oneByte struct {
	f *os.File
	n int
}

func (r *oneByte) Read(p []byte) (int, error) {
	if len(p) == 0 {
		return 0, nil
	}
	n, err := r.f.Read(p[:1])
	r.n += n
	return n, err
}

func (r *oneByte) Close() error { return r.f.Close() }

func main() {
	dir := os.Args[1]
	ents, err := os.ReadDir(dir)
	if err != nil {
		panic(err)
	}
	for _, e := range ents {
		var rd *oneByte
		ctxt := build.Default
		ctxt.OpenFile = func(p string) (io.ReadCloser, error) {
			f, err := os.Open(p)
			if err != nil {
				return nil, err
			}
			rd = &oneByte{f: f}
			return rd, nil
		}
		ok, err := ctxt.MatchFile(dir, e.Name())
		msg := ""
		if err != nil {
			msg = strings.ReplaceAll(err.Error(), dir, "DIR")
		}
		n := -1
		if rd != nil {
			n = rd.n
		}
		fmt.Printf("%s\t%d\t%t\t%q\n", e.Name(), n, ok, msg)
	}
}
"#;

/// Build [`PROBE`] with `toolchain`; the binary's path (kept alive by `dir`).
fn build_probe(dir: &Path, toolchain: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    write(dir, "go.mod", "module probe\n\ngo 1.21\n")?;
    write(dir, "main.go", PROBE)?;
    let bin = dir.join("probe");
    let out = go_cmd(toolchain)
        .args(["build", "-o"])
        .arg(&bin)
        .arg(".")
        .current_dir(dir)
        .output()
        .context("go build the probe")?;
    anyhow::ensure!(
        out.status.success(),
        "go build the probe ({toolchain:?}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(bin)
}

/// One line of [`PROBE`] output: (bytes read, matched, error).
type ProbeLine = (usize, bool, String);

fn probe(bin: &Path, dir: &Path) -> anyhow::Result<BTreeMap<String, ProbeLine>> {
    let out = std::process::Command::new(bin)
        .arg(dir)
        .output()
        .context("run the probe")?;
    anyhow::ensure!(out.status.success(), "probe failed: {out:?}");
    let mut lines = BTreeMap::new();
    for line in String::from_utf8(out.stdout)?.lines() {
        let f: Vec<&str> = line.splitn(4, '\t').collect();
        let [name, n, ok, err] = f.as_slice() else {
            anyhow::bail!("bad probe line {line:?}");
        };
        let n: i64 = n.parse()?;
        anyhow::ensure!(n >= 0, "{name}: go/build did not open it");
        lines.insert(
            name.to_string(),
            (usize::try_from(n)?, *ok == "true", err.to_string()),
        );
    }
    Ok(lines)
}

/// The T13 corpus: every adversarial header shape, as file contents.
fn t13_corpus() -> Vec<(&'static str, Vec<u8>)> {
    let ignore = b"//go:build ignore\n\nint x;\n".to_vec();
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(&ignore);
    // A `//` filler line padded so the header (through the code byte that ends
    // it) is exactly `len` bytes long.
    let header_of_len = |len: usize| {
        let mut b = b"//go:build ignore\n//".to_vec();
        b.resize(len - 2, b'x');
        b.extend_from_slice(b"\ni = 1;\n");
        b
    };
    let mut big = b"//go:build ignore\n\n".to_vec();
    big.resize(4 << 20, b'z');
    vec![
        ("empty.c", Vec::new()),
        ("no_comments.c", b"int x;\n".to_vec()),
        ("build_blank_code.c", ignore.clone()),
        (
            "plus_build_blank.c",
            b"// +build ignore\n\nint x;\n".to_vec(),
        ),
        (
            "plus_build_no_blank.c",
            b"// +build ignore\nint x;\n".to_vec(),
        ),
        (
            "block_multi.c",
            b"/* a\n   b */\n//go:build ignore\n\nint x;\n".to_vec(),
        ),
        (
            "block_unclosed.c",
            b"/* never closed\n//go:build ignore\n".to_vec(),
        ),
        (
            "block_in_line.c",
            b"// a /* b\n//go:build ignore\n\nint x;\n".to_vec(),
        ),
        (
            "block_then_code.c",
            b"/* x */ int y; //go:build ignore\n".to_vec(),
        ),
        ("eof_no_newline.c", b"//go:build ignore".to_vec()),
        ("crlf.c", b"//go:build ignore\r\n\r\nint x;\r\n".to_vec()),
        ("lone_cr.c", b"//go:build ignore\rint x;\n".to_vec()),
        ("bom.c", bom),
        ("bom_partial.c", b"\xef\xbbx\n".to_vec()),
        ("bom_first_byte.c", b"\xef".to_vec()),
        (
            "bad_utf8.c",
            b"// \xff\xfe bad\n//go:build ignore\n\nint x;\n".to_vec(),
        ),
        ("nul.c", b"//go:build ignore\n\x00\nint x;\n".to_vec()),
        (
            "nul_in_comment.c",
            b"// a\x00b\n//go:build ignore\n".to_vec(),
        ),
        (
            "leading_blank.c",
            b"\n\n\t \n//go:build ignore\n\nint x;\n".to_vec(),
        ),
        ("late_constraint.c", b"int x;\n//go:build ignore\n".to_vec()),
        ("stray_slash.c", b"/x\n//go:build ignore\n".to_vec()),
        (
            "semicolons.c",
            b";; ;\n//go:build ignore\n\nint x;\n".to_vec(),
        ),
        ("whitespace_only.c", b"  \n\t\n".to_vec()),
        (
            "both_forms.s",
            b"//go:build ignore\n// +build ignore\n\nTEXT x\n".to_vec(),
        ),
        ("chunk_minus_one.c", header_of_len(4095)),
        ("chunk_exact.c", header_of_len(4096)),
        ("chunk_plus_one.c", header_of_len(4097)),
        ("big.c", big),
    ]
}

/// T13b: the extractor reads exactly the bytes the real `go/build` reads, file
/// by file, on every Go the suite pins — and replacing everything after those
/// bytes changes neither what `go/build` reads nor what it decides.
///
/// `go/build` wraps the file in a `bufio.Reader` and first `Peek`s three bytes
/// for a BOM, so with a one-byte reader it pulls `max(prefix, min(3, len))`
/// bytes: the extractor's prefix, or the BOM probe of a shorter file.
#[tokio::test]
async fn t13b_the_header_extractor_matches_go_build() -> anyhow::Result<()> {
    require_go!();
    let corpus = t13_corpus();
    let tmp = tempfile::tempdir()?;
    let orig = tmp.path().join("orig");
    let mutated = tmp.path().join("mutated");
    let mut prefixes = BTreeMap::new();
    for (name, bytes) in &corpus {
        let e = plugingo::extract_go_build_header(bytes.as_slice())?;
        assert!(bytes.starts_with(&e), "{name}: the extract is not a prefix");
        std::fs::create_dir_all(&orig)?;
        std::fs::write(orig.join(name), bytes)?;
        // Everything past the prefix replaced by a body that would change the
        // verdict if go/build ever read it.
        let mut m = e.clone();
        if m.len() < bytes.len() {
            m.extend_from_slice(b"\n//go:build linux && !linux\n\x00/* tail */\n");
        }
        std::fs::create_dir_all(&mutated)?;
        std::fs::write(mutated.join(name), &m)?;
        prefixes.insert(name.to_string(), (e.len(), bytes.len()));
    }

    for toolchain in go_toolchains() {
        let tc = toolchain.as_deref();
        let bdir = tempfile::tempdir()?;
        let bin = build_probe(bdir.path(), tc)?;
        let got = probe(&bin, &orig)?;
        let got_mut = probe(&bin, &mutated)?;
        assert_eq!(got.len(), corpus.len(), "{tc:?}: every file probed");
        for (name, (n, ok, err)) in &got {
            let (plen, flen) = prefixes[name];
            assert_eq!(
                *n,
                plen.max(flen.min(3)),
                "{tc:?} {name}: go/build read {n} bytes, the extractor kept {plen}"
            );
            assert_eq!(
                got_mut.get(name),
                Some(&(*n, *ok, err.clone())),
                "{tc:?} {name}: go/build's view changed with only the body past the extract"
            );
        }
    }
    Ok(())
}

/// The body mutations of T19: things that would mean something to cgo, embed or
/// the constraint parser, all after the header.
const BODY_MUTATION: &str = "\nimport \"C\"\n#cgo LDFLAGS: -lnothere\n//go:embed nothere\n//go:build ignore\n// +build ignore\n";

/// T19 (B4): for every header-read extension, mutating only what follows the
/// header leaves `go list -json` (every field, both `CGO_ENABLED` values)
/// byte-identical — and, through heph, re-runs the header target and keeps
/// `_golist` a hit.
#[tokio::test]
async fn t19_body_mutations_leave_go_list_byte_identical() -> anyhow::Result<()> {
    require_go!();
    let header = "//go:build !nothere\n\n";
    let mut files: Vec<(String, String)> = vec![
        ("go.mod".to_string(), GO_MOD.to_string()),
        ("p/p.go".to_string(), "package p\n".to_string()),
    ];
    for ext in plugingo::GO_HEADER_READ_EXTS {
        files.push((format!("p/x_{ext}.{ext}"), format!("{header}body\n")));
    }
    let dir = tree_owned(&files)?;
    let ws = make_workspace(dir)?;
    let (before, _) = golist(&ws, "p").await?;

    let list = |toolchain: Option<&str>, cgo: &str| -> anyhow::Result<Vec<u8>> {
        let out = go_cmd(toolchain)
            .args(["list", "-e", "-json", "./p"])
            .env("CGO_ENABLED", cgo)
            .current_dir(ws.dir.path())
            .output()
            .context("go list")?;
        anyhow::ensure!(
            out.status.success(),
            "go list ({toolchain:?}, CGO_ENABLED={cgo}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // `Stale`/`StaleReason` report the state of Go's build cache, which
        // the other listings in this loop move; `_golist` never selects them.
        Ok(String::from_utf8(out.stdout)?
            .lines()
            .filter(|l| !l.trim_start().starts_with("\"Stale"))
            .flat_map(|l| [l, "\n"])
            .collect::<String>()
            .into_bytes())
    };
    let mut lists = Vec::new();
    for toolchain in go_toolchains() {
        for cgo in ["0", "1"] {
            lists.push((toolchain.clone(), cgo, list(toolchain.as_deref(), cgo)?));
        }
    }

    for ext in plugingo::GO_HEADER_READ_EXTS {
        write(
            ws.dir.path(),
            &format!("p/x_{ext}.{ext}"),
            &format!("{header}body\n{BODY_MUTATION}"),
        )?;
    }
    for (toolchain, cgo, want) in &lists {
        let got = list(toolchain.as_deref(), cgo)?;
        assert!(
            got == *want,
            "{toolchain:?} CGO_ENABLED={cgo}: go list changed with only bodies edited:\n\
             before: {}\nafter: {}",
            String::from_utf8_lossy(want),
            String::from_utf8_lossy(&got)
        );
    }

    let ws = reopen(ws)?;
    let (after, ev) = golist(&ws, "p").await?;
    ev.assert_executed("@heph/go/headers/p", "headers", "every body changed");
    ev.assert_hit("p", "_golist", "no header changed");
    assert_eq!(pkg_error(&after), pkg_error(&before));
    Ok(())
}

/// The entry of a `go list -json` stream whose `ImportPath` is `import_path`,
/// verbatim — what `_golist` keeps of it (it drops the synthetic `-test`
/// variants: `p.test`, `p [p.test]`, `p_test [p.test]`).
fn golist_entry(stream: &[u8], import_path: &str) -> anyhow::Result<String> {
    let text = std::str::from_utf8(stream)?;
    let want = format!("\t\"ImportPath\": \"{import_path}\",");
    let mut found = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        cur.push_str(line);
        if line == "}\n" {
            if cur.lines().any(|l| l == want) {
                found.push(std::mem::take(&mut cur));
            }
            cur.clear();
        }
    }
    anyhow::ensure!(found.len() == 1, "{import_path}: {found:?} in {text}");
    Ok(found.remove(0))
}

/// G14: the `.go` analogue of T19. For each body mutation that would mean
/// something to the parser, cgo, the constraint parser or embed, `go list` run
/// as `_golist` runs it (its fields, `-e -test`) reports a byte-identical entry
/// for the package, under both `CGO_ENABLED` values and on every Go the suite
/// pins — and the extract does not move. Through heph, `_golist` stays a hit.
///
/// The synthetic `p.test` entry is not held to this, and it does change: `-test`
/// parses the `_test.go` files whole to find the test functions, so a syntax
/// error in a test body shows up in its `Error`. `_golist` never keeps it.
#[tokio::test]
async fn g14_go_body_mutations_leave_go_list_byte_identical() -> anyhow::Result<()> {
    require_go!();
    const IMPORT_PATH: &str = "example.com/lanes/lib";
    let dir = lib_pkg()?;
    let ws = make_workspace(dir)?;
    let (before, _) = golist(&ws, "lib").await?;

    let list = |toolchain: Option<&str>, cgo: &str| -> anyhow::Result<String> {
        let out = go_cmd(toolchain)
            .args([
                "list",
                plugingo::GOLIST_JSON_FIELDS,
                "-e",
                "-test",
                IMPORT_PATH,
            ])
            .env("CGO_ENABLED", cgo)
            .current_dir(ws.dir.path())
            .output()
            .context("go list")?;
        anyhow::ensure!(
            out.status.success(),
            "go list ({toolchain:?}, CGO_ENABLED={cgo}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        golist_entry(&out.stdout, IMPORT_PATH)
    };
    let mut lists = Vec::new();
    for toolchain in go_toolchains() {
        for cgo in ["0", "1"] {
            lists.push((toolchain.clone(), cgo, list(toolchain.as_deref(), cgo)?));
        }
    }

    let mutations: &[(&str, &str, &str)] = &[
        (
            "a syntax error in a _test.go body",
            "lib/lib_test.go",
            "\nfunc broken( {\n",
        ),
        ("a late import \"C\"", "lib/lib.go", "\nimport \"C\"\n"),
        ("a late //go:build", "lib/lib.go", "\n//go:build ignore\n"),
        (
            "//go:embed without an embed import",
            "lib/lib.go",
            "\n//go:embed nothere\nvar s string\n",
        ),
    ];
    let base = |rel: &str| {
        if rel.ends_with("_test.go") {
            LIB_TEST_GO
        } else {
            LIB_GO
        }
    };
    for (what, rel, tail) in mutations {
        let mutated = format!("{}{tail}", base(rel));
        assert_eq!(
            plugingo::extract_go_view(base(rel).as_bytes())?,
            plugingo::extract_go_view(mutated.as_bytes())?,
            "{what}: the extract must not move"
        );
        write(ws.dir.path(), rel, &mutated)?;
        for (toolchain, cgo, want) in &lists {
            let got = list(toolchain.as_deref(), cgo)?;
            assert!(
                got == *want,
                "{what} ({toolchain:?}, CGO_ENABLED={cgo}): go list changed:\n\
                 before: {want}\nafter: {got}"
            );
        }
        write(ws.dir.path(), rel, base(rel))?;
    }

    for (_, rel, tail) in mutations {
        let cur = std::fs::read_to_string(ws.dir.path().join(rel))?;
        write(ws.dir.path(), rel, &format!("{cur}{tail}"))?;
    }
    let ws = reopen(ws)?;
    let (after, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("@heph/go/headers/lib", "headers", "every body changed");
    ev.assert_hit("lib", "_golist", "no view changed");
    assert_eq!(pkg_error(&after), pkg_error(&before));
    Ok(())
}

// ---------------------------------------------------------------------------
// G1–G6: `.go` files reach `_golist` through their go/build view
// ---------------------------------------------------------------------------

const LIB_GO: &str = "package lib\n\nimport \"fmt\"\n\nfunc F() string { return fmt.Sprint(1) }\n";
const LIB_TEST_GO: &str =
    "package lib\n\nimport \"testing\"\n\nfunc TestF(t *testing.T) { _ = F() }\n";

fn lib_pkg() -> anyhow::Result<tempfile::TempDir> {
    tree(&[
        ("go.mod", GO_MOD),
        ("lib/lib.go", LIB_GO),
        ("lib/lib_test.go", LIB_TEST_GO),
    ])
}

/// G1: a function-body edit in a `.go` file keeps `_golist` a hit, and the
/// compile re-runs on the new bytes.
#[tokio::test]
async fn g1_a_go_body_edit_keeps_golist_and_recompiles() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(lib_pkg()?)?;
    run(&ws, "//lib:build_lib@v=host").await?;

    write(
        ws.dir.path(),
        "lib/lib.go",
        &LIB_GO.replace("Sprint(1)", "Sprint(2)"),
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = run(&ws, "//lib:build_lib@v=host").await?;
    ev.assert_executed("@heph/go/headers/lib", "headers", "lib.go changed");
    ev.assert_hit("lib", "_golist", "only a body changed");
    ev.assert_executed("lib", "build_lib", "the compile reads the whole file");
    Ok(())
}

/// G2: the same for a `_test.go` body: `go list -test` full-parses it only for
/// the synthetic test main, which `_golist` drops.
#[tokio::test]
async fn g2_a_test_go_body_edit_keeps_golist() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(lib_pkg()?)?;
    golist(&ws, "lib").await?;

    write(
        ws.dir.path(),
        "lib/lib_test.go",
        &LIB_TEST_GO.replace("_ = F()", "if F() == \"\" { t.Fatal() }"),
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("@heph/go/headers/lib", "headers", "lib_test.go changed");
    ev.assert_hit("lib", "_golist", "only a test body changed");
    Ok(())
}

/// G3: adding an import re-lists, and the new import is reported.
#[tokio::test]
async fn g3_adding_an_import_relists() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(lib_pkg()?)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.imports, vec!["fmt"]);

    write(
        ws.dir.path(),
        "lib/lib.go",
        &LIB_GO
            .replace("import \"fmt\"", "import (\n\t\"fmt\"\n\t\"strings\"\n)")
            .replace("fmt.Sprint(1)", "strings.ToUpper(fmt.Sprint(1))"),
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "_golist", "an import was added");
    assert_eq!(pkg.imports, vec!["fmt", "strings"]);
    Ok(())
}

/// G4: adding a `//go:embed` pattern below the imports re-lists, and the new
/// file is embedded.
#[tokio::test]
async fn g4_adding_an_embed_pattern_relists() -> anyhow::Result<()> {
    require_go!();
    let src = "package lib\n\nimport _ \"embed\"\n\nvar x = 1\n\n//go:embed a.txt\nvar a string\n";
    let dir = tree(&[
        ("go.mod", GO_MOD),
        ("lib/lib.go", src),
        ("lib/a.txt", "a"),
        ("lib/b.txt", "b"),
    ])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.embed_patterns, vec!["a.txt"]);

    write(
        ws.dir.path(),
        "lib/lib.go",
        &format!("{src}\n//go:embed b.txt\nvar b string\n"),
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "_golist", "an embed pattern was added");
    assert_eq!(pkg.embed_patterns, vec!["a.txt", "b.txt"]);
    Ok(())
}

/// G5: flipping a `.go` file's build constraint re-lists, and the file leaves
/// `GoFiles`.
#[tokio::test]
async fn g5_flipping_a_go_build_constraint_relists() -> anyhow::Result<()> {
    require_go!();
    let dir = tree(&[
        ("go.mod", GO_MOD),
        ("lib/lib.go", LIB_GO),
        ("lib/extra.go", "package lib\n\nfunc G() {}\n"),
    ])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.go_files, vec!["extra.go", "lib.go"]);

    write(
        ws.dir.path(),
        "lib/extra.go",
        "//go:build ignore\n\npackage lib\n\nfunc G() {}\n",
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "_golist", "extra.go gained an ignore constraint");
    assert_eq!(pkg.go_files, vec!["lib.go"]);
    Ok(())
}

/// G6: in a vendored module the `.go` files stay content-hashed, so a body
/// edit re-lists.
#[tokio::test]
async fn g6_a_vendored_module_relists_on_a_go_body_edit() -> anyhow::Result<()> {
    require_go!();
    let dir = tree_owned(&vendored_module("", "example.com/vend", "1.21"))?;
    let ws = make_workspace(dir)?;
    golist(&ws, "").await?;

    write(ws.dir.path(), "lib.go", "package lib\n\nfunc F() {}\n")?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "").await?;
    ev.assert_executed("", "_golist", "a vendored module keeps the content lane");
    Ok(())
}

// ---------------------------------------------------------------------------
// G8–G12: generated files reach `_golist` through the header target
// ---------------------------------------------------------------------------

const GEN_LIB_GO: &str = "package lib\n\nfunc F() string { return G() }\n";
const GEN_GO: &str = "package lib\n\nimport \"fmt\"\n\nfunc G() string { return fmt.Sprint(1) }\n";

/// A `bash` codegen target `name` in a BUILD file writing each `(out, contents)`.
/// `go_src` labels it for the `go_src` query; without it, only a
/// `go_codegen_deps` entry reaches it.
fn gen_target(name: &str, outs: &[(&str, &str)], go_src: bool) -> String {
    let mut run = Vec::new();
    for (out, contents) in outs {
        run.push(format!("mkdir -p $(dirname {out:?}) && : > {out:?}"));
        for line in contents.lines() {
            // Single-quoted: bash keeps tabs, `"` and `\` as they are.
            let line = line.replace('\'', "'\\''");
            run.push(format!("printf '%s\\n' '{line}' >> {out:?}"));
        }
    }
    let outs: Vec<&str> = outs.iter().map(|(o, _)| *o).collect();
    let labels = if go_src { "[\"go_src\"]" } else { "[]" };
    format!(
        "target(\n    name = {name:?},\n    driver = \"bash\",\n    run = {run:?},\n    \
         out = {outs:?},\n    labels = {labels},\n    codegen = \"copy\",\n)\n"
    )
}

/// The root module with `lib/lib.go` and a `go_src`-labelled generator in
/// `lib/BUILD` writing `outs`.
fn gen_pkg(outs: &[(&str, &str)]) -> anyhow::Result<tempfile::TempDir> {
    tree(&[
        ("go.mod", GO_MOD),
        ("lib/lib.go", GEN_LIB_GO),
        ("lib/BUILD", &gen_target("gen", outs, true)),
    ])
}

/// Rewrite `lib/BUILD`'s generator to write `outs`. The BUILD file itself is a
/// non-Go file of the package, keyed by name only: the key moves only through
/// what the generator now writes.
fn regen(root: &Path, outs: &[(&str, &str)]) -> anyhow::Result<()> {
    write(root, "lib/BUILD", &gen_target("gen", outs, true))
}

/// G8: a generated `.go` file's function body changes, its imports do not: the
/// generator and the header target re-run, `_golist` is a hit, and the compile
/// re-runs on the new bytes.
#[tokio::test]
async fn g8_a_generated_go_body_edit_keeps_golist_and_recompiles() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(gen_pkg(&[("gen.go", GEN_GO)])?)?;
    let (_, ev) = run(&ws, "//lib:build_lib@v=host").await?;
    ev.assert_executed("lib", "gen", "first build");

    regen(
        ws.dir.path(),
        &[("gen.go", &GEN_GO.replace("Sprint(1)", "Sprint(2)"))],
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = run(&ws, "//lib:build_lib@v=host").await?;
    ev.assert_executed("lib", "gen", "the generator changed");
    ev.assert_executed("@heph/go/headers/lib", "headers", "gen.go changed");
    ev.assert_hit("lib", "_golist", "only a generated body changed");
    ev.assert_executed("lib", "build_lib", "the compile reads the whole file");
    Ok(())
}

/// G9: a generated `.go` file gains an import: `_golist` re-runs and lists it.
#[tokio::test]
async fn g9_a_generated_go_import_relists() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(gen_pkg(&[("gen.go", GEN_GO)])?)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.imports, vec!["fmt"]);

    regen(
        ws.dir.path(),
        &[(
            "gen.go",
            &GEN_GO
                .replace("import \"fmt\"", "import (\n\t\"fmt\"\n\t\"strings\"\n)")
                .replace("fmt.Sprint(1)", "strings.ToUpper(fmt.Sprint(1))"),
        )],
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "_golist", "a generated file gained an import");
    assert_eq!(pkg.imports, vec!["fmt", "strings"]);
    Ok(())
}

/// G10: the generator writes a second `.go` file: `_golist` re-runs and lists it.
#[tokio::test]
async fn g10_a_new_generated_go_file_relists() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(gen_pkg(&[("gen.go", GEN_GO)])?)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.go_files, vec!["gen.go", "lib.go"]);

    regen(
        ws.dir.path(),
        &[
            ("gen.go", GEN_GO),
            ("gen2.go", "package lib\n\nfunc H() {}\n"),
        ],
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "_golist", "the generator added a .go file");
    assert_eq!(pkg.go_files, vec!["gen.go", "gen2.go", "lib.go"]);
    Ok(())
}

/// G11 (E1 kept): a generated non-Go file's content changes, nothing else does:
/// `_golist` re-runs, because a generated non-Go file is keyed by its whole
/// content.
#[tokio::test]
async fn g11_a_generated_non_go_content_edit_relists() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(gen_pkg(&[("gen.go", GEN_GO), ("data.txt", "one")])?)?;
    golist(&ws, "lib").await?;

    regen(ws.dir.path(), &[("gen.go", GEN_GO), ("data.txt", "two")])?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("@heph/go/headers/lib", "headers", "data.txt changed");
    ev.assert_executed(
        "lib",
        "_golist",
        "a generated non-Go file is keyed by content",
    );
    Ok(())
}

/// G12: the `go_codegen_deps` lane — a generator with no `go_src` label, named
/// from the root package's `provider_state` — is keyed the same way: a body edit
/// keeps `_golist`, an import re-lists.
#[tokio::test]
async fn g12_a_go_codegen_deps_generator_is_keyed_by_go_view() -> anyhow::Result<()> {
    require_go!();
    let build = |src: &str| gen_target("gen", &[("gen.go", src)], false);
    let dir = tree(&[
        ("go.mod", GO_MOD),
        (
            "BUILD",
            "provider_state(provider = \"go\", go_codegen_deps = [\"//lib:gen\"])\n",
        ),
        ("lib/lib.go", GEN_LIB_GO),
        ("lib/BUILD", &build(GEN_GO)),
    ])?;
    let ws = make_workspace(dir)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.go_files, vec!["gen.go", "lib.go"]);

    write(
        ws.dir.path(),
        "lib/BUILD",
        &build(&GEN_GO.replace("Sprint(1)", "Sprint(2)")),
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "gen", "the generator changed");
    ev.assert_hit("lib", "_golist", "only a generated body changed");

    write(
        ws.dir.path(),
        "lib/BUILD",
        &build(&GEN_GO.replace("import \"fmt\"", "import (\n\t\"fmt\"\n\t_ \"strings\"\n)")),
    )?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "_golist", "a generated file gained an import");
    assert_eq!(pkg.imports, vec!["fmt", "strings"]);
    Ok(())
}

/// I3: a checked-in `.go` file and a generator output at the same path fail the
/// build rather than key only one of them. The engine's sandbox collision check
/// catches it in the header target before `build_headers_tar` would; either
/// error names the path.
#[tokio::test]
async fn g13_a_checked_in_file_shadowing_a_generated_one_fails() -> anyhow::Result<()> {
    require_go!();
    let dir = gen_pkg(&[("gen.go", GEN_GO)])?;
    write(dir.path(), "lib/gen.go", GEN_GO)?;
    let ws = make_workspace(dir)?;
    let err = golist(&ws, "lib")
        .await
        .err()
        .context("a path staged by two lanes must fail")?;
    let msg = format!("{err:#}");
    assert!(
        msg.contains("lib/gen.go")
            && (msg.contains("is both checked in and produced by")
                || msg.contains("output collision")),
        "unexpected error: {msg}"
    );
    Ok(())
}

/// G15: the generator stops writing one of its `.go` files: `_golist` re-runs
/// and the file leaves `GoFiles`. Removal moves the key as addition does (G10).
#[tokio::test]
async fn g15_a_removed_generated_go_file_relists() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(gen_pkg(&[
        ("gen.go", GEN_GO),
        ("gen2.go", "package lib\n\nfunc H() {}\n"),
    ])?)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.go_files, vec!["gen.go", "gen2.go", "lib.go"]);

    regen(ws.dir.path(), &[("gen.go", GEN_GO)])?;
    let ws = reopen(ws)?;
    let (pkg, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("@heph/go/headers/lib", "headers", "gen2.go is gone");
    ev.assert_executed("lib", "_golist", "the generator dropped a .go file");
    assert_eq!(pkg.go_files, vec!["gen.go", "lib.go"]);
    Ok(())
}

/// G16: a warm run over an unchanged tree executes nothing but the tree reads —
/// the header target and `_golist` are both hits, as is everything around them.
/// A header target
/// that re-ran on every invocation would hold this red even with `_golist` a
/// hit.
#[tokio::test]
async fn g16_a_warm_no_op_run_executes_nothing() -> anyhow::Result<()> {
    require_go!();
    let dir = gen_pkg(&[("gen.go", GEN_GO)])?;
    write(
        dir.path(),
        "lib/lib_test.go",
        "package lib\n\nimport \"testing\"\n\nfunc TestF(t *testing.T) { _ = F() }\n",
    )?;
    write(dir.path(), "lib/x.h", "// a header\n\nint x;\n")?;
    let ws = make_workspace(dir)?;
    let (_, ev) = run(&ws, "//lib:build_lib@v=host").await?;
    ev.assert_executed("lib", "_golist", "first build");

    let ws = reopen(ws)?;
    let (_, ev) = run(&ws, "//lib:build_lib@v=host").await?;
    ev.assert_hit("@heph/go/headers/lib", "headers", "nothing changed");
    ev.assert_hit("lib", "_golist", "nothing changed");
    // `//@heph/fs:*` are the reads of the tree itself, which every run makes
    // to find out that nothing changed; everything else must be a hit.
    let executed: Vec<String> = ev
        .touched()
        .into_iter()
        .filter(|t| t.starts_with("exec") && !t.starts_with("exec //@heph/fs:"))
        .collect();
    assert!(executed.is_empty(), "a no-op run executed {executed:#?}");
    Ok(())
}

/// G17: a generated `_test.go` file's body changes, its imports do not: the
/// header target re-runs on the new bytes and `_golist` is a hit.
#[tokio::test]
async fn g17_a_generated_test_go_body_edit_keeps_golist() -> anyhow::Result<()> {
    require_go!();
    let test_go = "package lib\n\nimport \"testing\"\n\nfunc TestG(t *testing.T) { _ = G() }\n";
    let ws = make_workspace(gen_pkg(&[("gen.go", GEN_GO), ("gen_test.go", test_go)])?)?;
    let (pkg, _) = golist(&ws, "lib").await?;
    assert_eq!(pkg.test_go_files, vec!["gen_test.go"]);

    regen(
        ws.dir.path(),
        &[
            ("gen.go", GEN_GO),
            ("gen_test.go", &test_go.replace("_ = G()", "t.Log(G())")),
        ],
    )?;
    let ws = reopen(ws)?;
    let (_, ev) = golist(&ws, "lib").await?;
    ev.assert_executed("lib", "gen", "the generator changed");
    ev.assert_executed("@heph/go/headers/lib", "headers", "gen_test.go changed");
    ev.assert_hit("lib", "_golist", "only a generated test body changed");
    Ok(())
}

/// `go/build`'s view of every package directory of a tree: for each, the
/// `ImportDir` result as JSON (with `-test` files and positions), and the bytes
/// each `.go` file read, through a one-byte reader.
const GO_VIEW_PROBE: &str = r#"package main

import (
	"encoding/json"
	"fmt"
	"go/build"
	"io"
	"os"
	"path/filepath"
)

type oneByte struct {
	f *os.File
	n *int
}

func (r *oneByte) Read(p []byte) (int, error) {
	if len(p) == 0 {
		return 0, nil
	}
	n, err := r.f.Read(p[:1])
	*r.n += n
	return n, err
}

func (r *oneByte) Close() error { return r.f.Close() }

func main() {
	root := os.Args[1]
	ents, err := os.ReadDir(root)
	if err != nil {
		panic(err)
	}
	for _, e := range ents {
		dir := filepath.Join(root, e.Name())
		read := map[string]*int{}
		ctxt := build.Default
		ctxt.CgoEnabled = true
		ctxt.OpenFile = func(p string) (io.ReadCloser, error) {
			f, err := os.Open(p)
			if err != nil {
				return nil, err
			}
			n := new(int)
			read[filepath.Base(p)] = n
			return &oneByte{f: f, n: n}, nil
		}
		pkg, err := ctxt.ImportDir(dir, 0)
		msg := ""
		if err != nil {
			msg = err.Error()
		}
		counts := map[string]int{}
		for k, v := range read {
			counts[k] = *v
		}
		pkg.Dir = ""
		view, _ := json.Marshal(struct {
			Pkg *build.Package
			Err string
		}{pkg, msg})
		reads := ""
		for k, v := range counts {
			reads += fmt.Sprintf("%s=%d,", k, v)
		}
		fmt.Printf("%s\t%s\t%s\n", e.Name(), view, reads)
	}
}
"#;

/// The G7 corpus: `.go` files of every header and embed shape, each in its own
/// package directory, as (dir, file, contents).
fn g7_corpus() -> Vec<(&'static str, &'static str, String)> {
    let body = "\nfunc F() int { return 1 }\n";
    vec![
        ("plain", "a.go", format!("package p\n{body}")),
        (
            "imports",
            "a.go",
            format!(
                "package p\n\nimport (\n\t\"fmt\"\n\tx \"os\"\n)\n\nvar _ = fmt.Sprint\nvar _ = x.Args\n{body}"
            ),
        ),
        (
            "constraint",
            "a.go",
            format!("//go:build linux || darwin\n\npackage p\n{body}"),
        ),
        (
            "ignored",
            "a.go",
            format!("//go:build ignore\n\npackage p\n{body}"),
        ),
        (
            "late_tag",
            "a.go",
            format!("package p\n//go:build ignore\n{body}"),
        ),
        ("bom", "a.go", format!("\u{feff}package p\n{body}")),
        (
            "crlf",
            "a.go",
            format!("package p\r\n\r\nimport \"fmt\"\r\n\r\nvar _ = fmt.Sprint\r\n{body}"),
        ),
        (
            "cgo",
            "a.go",
            format!(
                "package p\n\n// #cgo LDFLAGS: -lm\n// #include <math.h>\nimport \"C\"\n{body}"
            ),
        ),
        (
            "directive",
            "a.go",
            format!("//go:debug panicnil=1\n\npackage p\n{body}"),
        ),
        (
            "embed",
            "a.go",
            format!(
                "package p\n\nimport _ \"embed\"\n\nvar s = \"//go:embed not_this\"\n\n//go:embed a.txt\nvar a string\n{body}"
            ),
        ),
        (
            "embed_line",
            "a.go",
            format!(
                "package p\n\nimport _ \"embed\"\n\n//line x.go:100\n//go:embed missing.txt\nvar a string\n{body}"
            ),
        ),
        (
            "test_file",
            "a_test.go",
            format!("package p\n\nimport \"testing\"\n\nfunc TestF(t *testing.T) {{}}\n{body}"),
        ),
        (
            "nonascii",
            "a.go",
            format!("// héllo\npackage p\n\nimport ü \"fmt\"\n\nvar _ = ü.Sprint\n{body}"),
        ),
        (
            "nul",
            "a.go",
            format!("package p\n\0import \"fmt\"\n{body}"),
        ),
        ("empty", "a.go", String::new()),
        (
            "no_imports",
            "a.go",
            format!("package p\n\nconst C = 1\n{body}"),
        ),
        (
            "embed_grouped",
            "a.go",
            format!(
                "package p\n\nimport (\n\t\"fmt\"\n\t_ \"embed\"\n)\n\nvar _ = fmt.Sprint\n\n//go:embed a.txt\nvar a string\n{body}"
            ),
        ),
        (
            "embed_aliased",
            "a.go",
            format!("package p\n\nimport em \"embed\"\n\n//go:embed a.txt\nvar a em.FS\n{body}"),
        ),
        (
            "directives",
            "a.go",
            format!(
                "//go:build linux || darwin\n//go:debug panicnil=1\n\n// Package p.\npackage p // import \"example.com/p\"\n\n//go:generate echo hi\nimport \"fmt\"\n\nvar _ = fmt.Sprint\n{body}"
            ),
        ),
        (
            "embed_indented_trailing",
            "a.go",
            format!(
                "package p\n\nimport \"embed\"\n\n\t//go:embed a.txt\nvar a string\n\nvar b embed.FS //go:embed a.txt\n{body}"
            ),
        ),
    ]
}

/// What go/build reads of `src` by the extractor's account, through a reader
/// that hands it one byte at a time: the whole file for a `W` or `E` view, the
/// header and the byte that ended it for an `H` one (none after a NUL, which
/// is the last byte read) — and at least the three-byte BOM probe.
fn go_view_read_len(src: &[u8]) -> anyhow::Result<usize> {
    let v = plugingo::extract_go_view(src)?;
    let read = match v.split_first() {
        Some((b'H', header)) if header.len() < src.len() && header.last() != Some(&0) => {
            header.len() + 1
        }
        Some((b'H', header)) => header.len(),
        Some((b'W' | b'E', _)) => src.len(),
        other => anyhow::bail!("unexpected view tag: {other:?}"),
    };
    Ok(read.max(src.len().min(3)))
}

/// Bytes appended past what go/build reads: a constraint that would exclude the
/// file, a NUL that would fail it, a comment. None is part of any view.
const HOSTILE_TAIL: &[u8] = b"\n//go:build linux && !linux\n\x00/* tail */\n";

/// The header mutations of G7's reverse property, each a find/replace on the
/// header region: wherever one changes `ImportDir`'s result, it must change the
/// extract.
const G7_HEADER_MUTATIONS: &[(&str, &str)] = &[
    ("package p", "package q"),
    ("\"fmt\"", "\"os\""),
    ("a.txt", "b.txt"),
    ("linux || darwin", "ignore"),
    ("panicnil=1", "panicnil=0"),
];

/// G7: the `.go` view extractor holds to the real `go/build`, on every Go the
/// suite pins:
///
/// - go/build reads exactly the bytes the extractor accounts for
///   ([`go_view_read_len`]), file by file;
/// - replacing everything past those bytes with [`HOSTILE_TAIL`] (appending it,
///   for a file go/build reads whole) leaves the extract and `ImportDir`'s whole
///   result (imports, positions, embeds, constraints, cgo directives, errors)
///   identical;
/// - the reverse: a header mutation ([`G7_HEADER_MUTATIONS`]) that changes
///   `ImportDir`'s result changes the extract.
///
/// A `W` file (header syntax error) is read whole and keyed whole, so it has no
/// tail to swap; it is in the corpus for the read count.
#[tokio::test]
async fn g7_the_go_view_extractor_matches_go_build() -> anyhow::Result<()> {
    require_go!();
    let corpus = g7_corpus();
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("tree");
    let put = |dir: &str, file: &str, bytes: &[u8]| -> anyhow::Result<()> {
        std::fs::create_dir_all(root.join(dir))?;
        std::fs::write(root.join(dir).join(file), bytes)?;
        Ok(())
    };
    let mut mutated: Vec<Vec<u8>> = Vec::new();
    let mut read_lens = BTreeMap::new();
    for (dir, file, src) in &corpus {
        let src = src.as_bytes();
        let n = go_view_read_len(src)?;
        let m = match plugingo::extract_go_view(src)?.first() {
            Some(b'W') => src.to_vec(),
            Some(b'E') => [src, HOSTILE_TAIL].concat(),
            _ if n < src.len() => [src.get(..n).unwrap_or(src), HOSTILE_TAIL].concat(),
            _ => src.to_vec(),
        };
        assert_eq!(
            plugingo::extract_go_view(src)?,
            plugingo::extract_go_view(m.as_slice())?,
            "{dir}: the hostile tail must keep the extract"
        );
        mutated.push(m);
        read_lens.insert(dir.to_string(), (*file, n));
        put(dir, "a.txt", b"a")?;
    }

    for toolchain in go_toolchains() {
        let tc = toolchain.as_deref();
        let bdir = tempfile::tempdir()?;
        write(bdir.path(), "go.mod", "module probe\n\ngo 1.21\n")?;
        write(bdir.path(), "main.go", GO_VIEW_PROBE)?;
        let bin = bdir.path().join("probe");
        let out = go_cmd(tc)
            .args(["build", "-o"])
            .arg(&bin)
            .arg(".")
            .current_dir(bdir.path())
            .output()
            .context("go build the probe")?;
        anyhow::ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // dir -> (go/build's view, bytes read per file)
        let probe = || -> anyhow::Result<BTreeMap<String, (String, String)>> {
            let out = std::process::Command::new(&bin).arg(&root).output()?;
            anyhow::ensure!(out.status.success(), "probe: {out:?}");
            Ok(String::from_utf8(out.stdout)?
                .lines()
                .filter_map(|l| {
                    let mut f = l.splitn(3, '\t');
                    Some((f.next()?, f.next()?, f.next()?))
                })
                .map(|(a, b, c)| (a.to_string(), (b.to_string(), c.to_string())))
                .collect())
        };
        for (dir, file, src) in &corpus {
            put(dir, file, src.as_bytes())?;
        }
        let before = probe()?;
        assert_eq!(before.len(), corpus.len(), "{tc:?}: every package probed");
        for (dir, (_, reads)) in &before {
            let (file, want) = read_lens[dir];
            let n: usize = reads
                .split(',')
                .find_map(|kv| kv.strip_prefix(&format!("{file}=")))
                .with_context(|| format!("{dir}: {file} was not opened"))?
                .parse()?;
            assert_eq!(
                n, want,
                "{tc:?} {dir}: go/build read {n} bytes, the extractor accounts for {want}"
            );
        }

        for ((dir, file, _), m) in corpus.iter().zip(&mutated) {
            put(dir, file, m)?;
        }
        let after = probe()?;
        for (dir, (view, _)) in &before {
            assert_eq!(
                after.get(dir).map(|(v, _)| v),
                Some(view),
                "{tc:?} {dir}: go/build's view changed with only the bytes past the extract"
            );
        }

        // The reverse property, one mutation per round.
        let mut checked = 0;
        for (from, to) in G7_HEADER_MUTATIONS {
            let mut changed = Vec::new();
            for (dir, file, src) in &corpus {
                let m = src.replacen(from, to, 1);
                put(dir, file, m.as_bytes())?;
                if m != *src {
                    changed.push((*dir, src.as_str(), m));
                }
            }
            let got = probe()?;
            for (dir, src, m) in changed {
                if got.get(dir).map(|(v, _)| v) == before.get(dir).map(|(v, _)| v) {
                    continue;
                }
                checked += 1;
                assert_ne!(
                    plugingo::extract_go_view(src.as_bytes())?,
                    plugingo::extract_go_view(m.as_bytes())?,
                    "{tc:?} {dir}: `{from}` -> `{to}` changed go/build's view but not the extract"
                );
            }
        }
        assert!(
            checked >= G7_HEADER_MUTATIONS.len(),
            "{tc:?}: the reverse property checked only {checked} cases"
        );
        for (dir, file, src) in &corpus {
            put(dir, file, src.as_bytes())?;
        }
    }
    Ok(())
}
