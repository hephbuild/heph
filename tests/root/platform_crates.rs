//! `scripts/platform-crates` — which crates' tests run on darwin/arm64 and
//! linux/arm64.
//!
//! CI runs the whole suite on linux/amd64 only. The other two platforms run
//! the tests of the crates this script selects, so a selection that misses a
//! crate with platform-conditional code is a platform whose code is compiled
//! but never tested there — and nothing goes red. Each test builds a small
//! fixture workspace and asserts what the script selects from it; the
//! adversarial inputs (near-miss keys, strings, nested packages, symlinks,
//! `target/`, non-UTF-8 bytes) are the ways a text scan goes wrong.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/platform-crates")
}

/// Through `python3`, not the shebang: CI's test jobs get the tree from an
/// `actions/download-artifact`, which drops the executable bit.
fn script_command() -> Command {
    let mut cmd = Command::new("python3");
    cmd.arg(script());
    cmd
}

/// A throwaway cargo workspace. Every fixture has an `e2e` member, because the
/// script always selects it and refuses a workspace without it.
struct Fixture {
    dir: tempfile::TempDir,
    members: Vec<String>,
    root_package: bool,
}

impl Fixture {
    /// A virtual workspace: the root directory belongs to no package.
    fn new() -> Self {
        Self::with_root_package(false)
    }

    fn with_root_package(root_package: bool) -> Self {
        let fixture = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            members: Vec::new(),
            root_package,
        };
        if root_package {
            fixture.write("src/lib.rs", "");
        }
        fixture.with_crate("e2e", "")
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, rel: &str, content: &str) {
        self.write_bytes(rel, content.as_bytes());
    }

    fn write_bytes(&self, rel: &str, content: &[u8]) {
        let path = self.path().join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&path, content).expect("write fixture file");
    }

    /// A member at `crates/<name>` with `lib` as its `src/lib.rs`.
    fn with_crate(self, name: &str, lib: &str) -> Self {
        self.with_crate_manifest(name, lib, "")
    }

    fn with_crate_manifest(mut self, name: &str, lib: &str, manifest_extra: &str) -> Self {
        self.write(
            &format!("crates/{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{manifest_extra}"
            ),
        );
        self.write(&format!("crates/{name}/src/lib.rs"), lib);
        self.members.push(format!("crates/{name}"));
        self.write_manifest();
        self
    }

    fn write_manifest(&self) {
        let members = self
            .members
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let package = if self.root_package {
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n"
        } else {
            ""
        };
        self.write(
            "Cargo.toml",
            &format!("{package}[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"),
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        script_command()
            .args(args)
            .arg("--manifest-path")
            .arg(self.path().join("Cargo.toml"))
            .env_remove("GITHUB_ACTIONS")
            .output()
            .expect("run scripts/platform-crates")
    }

    /// The selected package names, from `--json`.
    fn selected(&self) -> Vec<String> {
        let out = self.run(&["--json"]);
        assert!(
            out.status.success(),
            "platform-crates failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("platform-crates --json is JSON");
        json.get("packages")
            .and_then(serde_json::Value::as_array)
            .expect("a packages array")
            .iter()
            .map(|v| v.as_str().expect("a name").to_string())
            .collect()
    }
}

fn assert_selected(selected: &[String], name: &str) {
    assert!(
        selected.iter().any(|s| s == name),
        "`{name}` should be selected; got {selected:?}"
    );
}

fn assert_not_selected(selected: &[String], name: &str) {
    assert!(
        !selected.iter().any(|s| s == name),
        "`{name}` should not be selected; got {selected:?}"
    );
}

#[test]
fn platform_crates_selects_cfg_owners() {
    let f = Fixture::new()
        .with_crate("plat", "#[cfg(target_os = \"linux\")]\npub fn f() {}\n")
        .with_crate("plain", "pub fn f() {}\n");
    let selected = f.selected();
    assert_selected(&selected, "plat");
    assert_not_selected(&selected, "plain");
}

/// A cfg in a nested package belongs to it, not to the package whose
/// directory contains it — the root package contains every member.
#[test]
fn platform_crates_does_not_attribute_nested_packages() {
    let f = Fixture::with_root_package(true).with_crate("inner", "#[cfg(unix)]\npub fn f() {}\n");
    let selected = f.selected();
    assert_selected(&selected, "inner");
    assert_not_selected(&selected, "root");
}

#[test]
fn platform_crates_counts_tests_dirs_and_cfg_macro() {
    let f = Fixture::new()
        .with_crate("macro", "pub fn f() -> bool { cfg ! ( unix ) }\n")
        .with_crate(
            "multiline",
            "#[cfg(all(\n    feature = \"x\",\n    unix\n))]\npub fn f() {}\n",
        )
        .with_crate("negated", "#[cfg(not(windows))]\npub fn f() {}\n")
        .with_crate(
            "anytest",
            "#[cfg(any(target_arch = \"aarch64\", test))]\npub fn f() {}\n",
        )
        .with_crate("innerattr", "#![cfg(unix)]\npub fn f() {}\n")
        .with_crate(
            "cfgattr",
            "#[cfg_attr(windows, derive(Debug))]\npub struct S;\n",
        )
        .with_crate(
            "cfgif",
            "cfg_if::cfg_if! {\n    if #[cfg(target_family = \"unix\")] {\n    }\n}\n",
        )
        .with_crate("intests", "")
        .with_crate("inbuild", "");
    f.write(
        "crates/intests/tests/it.rs",
        "#[cfg(target_os = \"macos\")]\n#[test]\nfn t() {}\n",
    );
    f.write(
        "crates/inbuild/build.rs",
        "fn main() { if cfg!(target_os = \"macos\") {} }\n",
    );
    let selected = f.selected();
    for name in [
        "macro",
        "multiline",
        "negated",
        "anytest",
        "innerattr",
        "cfgattr",
        "cfgif",
        "intests",
        "inbuild",
    ] {
        assert_selected(&selected, name);
    }
}

#[test]
fn platform_crates_ignores_near_misses() {
    let f = Fixture::new()
        .with_crate("feature", "#[cfg(feature = \"unix\")]\npub fn f() {}\n")
        .with_crate("testonly", "#[cfg(test)]\nmod tests {}\n")
        .with_crate(
            "ident",
            "pub fn f() -> &'static str { let target_os = \"linux\"; target_os }\n",
        )
        .with_crate(
            "suffix",
            "fn my_cfg(x: bool) -> bool { x }\npub fn f() -> bool { my_cfg(unix_like()) }\nfn unix_like() -> bool { true }\n",
        )
        .with_crate("prefixed", "#[cfg(my_target_os)]\npub fn f() {}\n")
        .with_crate(
            "otherconst",
            "pub fn f() -> &'static str { std::env::consts::DLL_EXTENSION }\n",
        )
        .with_crate(
            "rustccfg",
            "//! Its build script would print `cargo:rustc-cfg=x`; this is not one.\n",
        )
        .with_crate("dirs", "")
        .with_crate("symlinked", "");
    // Build output, fixtures and generated code never select.
    f.write(
        "crates/dirs/target/debug/build/x/out.rs",
        "#[cfg(unix)]\nfn f() {}\n",
    );
    f.write(
        "crates/dirs/testdata/case/src/lib.rs",
        "#[cfg(unix)]\nfn f() {}\n",
    );
    f.write("crates/dirs/src/gen/mod.rs", "#[cfg(unix)]\nfn f() {}\n");
    f.write("crates/dirs/.hidden/x.rs", "#[cfg(unix)]\nfn f() {}\n");
    // Symlinks are not followed: a link into a directory no package owns.
    f.write("outside/plat.rs", "#[cfg(unix)]\nfn f() {}\n");
    std::os::unix::fs::symlink(
        f.path().join("outside/plat.rs"),
        f.path().join("crates/symlinked/src/plat.rs"),
    )
    .expect("symlink a file");
    std::os::unix::fs::symlink(
        f.path().join("outside"),
        f.path().join("crates/symlinked/src/linked_dir"),
    )
    .expect("symlink a directory");

    let selected = f.selected();
    for name in [
        "feature",
        "testonly",
        "ident",
        "suffix",
        "prefixed",
        "otherconst",
        "rustccfg",
        "dirs",
        "symlinked",
    ] {
        assert_not_selected(&selected, name);
    }
    assert_eq!(selected, ["e2e"], "only the always-selected crate");
}

#[test]
fn platform_crates_selects_build_rs_rustc_cfg() {
    let f = Fixture::new()
        .with_crate("oldsyntax", "")
        .with_crate("newsyntax", "")
        .with_crate("readsenv", "");
    f.write(
        "crates/oldsyntax/build.rs",
        "fn main() { println!(\"cargo:rustc-cfg=has_x\"); }\n",
    );
    f.write(
        "crates/newsyntax/build.rs",
        "fn main() { println!(\"cargo::rustc-cfg=has_y\"); }\n",
    );
    f.write(
        "crates/readsenv/build.rs",
        "fn main() { let _ = std::env::var(\"CARGO_CFG_TARGET_ARCH\"); }\n",
    );
    let selected = f.selected();
    for name in ["oldsyntax", "newsyntax", "readsenv"] {
        assert_selected(&selected, name);
    }
}

/// A `target_*` key outside the known set selects its crate and warns, naming
/// the file and the key. (The spec first had this fail the run; board
/// amendment A7 changed it to select-and-warn, since over-selecting is the
/// safe direction and a red CI over a new cfg key helps nobody.)
#[test]
fn platform_crates_warns_on_unknown_target_key() {
    let f = Fixture::new()
        .with_crate(
            "ptrwidth",
            "\n#[cfg(target_pointer_width = \"64\")]\npub fn f() {}\n",
        )
        .with_crate("madeup", "#[cfg(target_foo)]\npub fn f() {}\n");
    let out = f.run(&[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.lines().any(|l| l == "-p ptrwidth"), "{stdout}");
    assert!(stdout.lines().any(|l| l == "-p madeup"), "{stdout}");
    assert!(
        stderr.contains("crates/ptrwidth/src/lib.rs:2") && stderr.contains("target_pointer_width"),
        "the warning names the file, line and key: {stderr}"
    );
    assert!(
        stderr.contains("crates/madeup/src/lib.rs:1") && stderr.contains("target_foo"),
        "the warning names the file, line and key: {stderr}"
    );
}

#[test]
fn platform_crates_selects_target_specific_dependencies() {
    let f = Fixture::new()
        .with_crate("helper", "")
        .with_crate_manifest(
            "cfgdep",
            "",
            "\n[target.'cfg(unix)'.dependencies]\nhelper = { path = \"../helper\" }\n",
        )
        .with_crate_manifest(
            "tripledep",
            "",
            "\n[target.x86_64-unknown-linux-gnu.dev-dependencies]\nhelper = { path = \"../helper\" }\n",
        )
        .with_crate_manifest(
            "plaindep",
            "",
            "\n[dependencies]\nhelper = { path = \"../helper\" }\n",
        );
    let selected = f.selected();
    assert_selected(&selected, "cfgdep");
    assert_selected(&selected, "tripledep");
    assert_not_selected(&selected, "plaindep");
    assert_not_selected(&selected, "helper");
}

#[test]
fn platform_crates_selects_env_consts() {
    let f = Fixture::new()
        .with_crate("os", "pub fn f() -> &'static str { std::env::consts::OS }\n")
        .with_crate(
            "grouped",
            "use std::env::consts::{\n    ARCH,\n    DLL_EXTENSION,\n};\npub fn f() -> (&'static str, &'static str) { (ARCH, DLL_EXTENSION) }\n",
        );
    let selected = f.selected();
    assert_selected(&selected, "os");
    assert_selected(&selected, "grouped");
}

/// A module kept outside the package directory still counts, through
/// `#[path]` or `include!`.
#[test]
fn platform_crates_follows_path_and_include() {
    let f = Fixture::new()
        .with_crate(
            "viapath",
            "#[path = \"../../../shared/plat.rs\"]\nmod plat;\n",
        )
        .with_crate("viainclude", "include!(\"../../../shared/included.rs\");\n");
    f.write("shared/plat.rs", "#[cfg(unix)]\npub fn f() {}\n");
    f.write(
        "shared/included.rs",
        "pub fn g() -> bool { cfg!(windows) }\n",
    );
    let selected = f.selected();
    assert_selected(&selected, "viapath");
    assert_selected(&selected, "viainclude");
}

/// Non-UTF-8 bytes, CRLF line endings and non-ASCII text before a cfg must
/// not stop the scan.
#[test]
fn platform_crates_reads_past_non_utf8_and_crlf() {
    let f = Fixture::new().with_crate("bytes", "");
    f.write_bytes(
        "crates/bytes/src/lib.rs",
        b"// caf\xc3\xa9 \xe4\xb8\xad \xff\xfe\r\n#[cfg(unix)]\r\npub fn f() {}\r\n",
    );
    assert_selected(&f.selected(), "bytes");
}

#[test]
fn platform_crates_never_selects_go_or_bin_e2e_and_always_e2e() {
    let f = Fixture::new()
        .with_crate("plugingo-e2e", "#[cfg(unix)]\npub fn f() {}\n")
        .with_crate("bin-e2e", "#[cfg(unix)]\npub fn f() {}\n");
    let selected = f.selected();
    assert_eq!(selected, ["e2e"]);
}

#[test]
fn platform_crates_fails_without_e2e() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"lonely\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write manifest");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/lib.rs"), "#[cfg(unix)]\nfn f() {}\n").expect("write lib");
    let out = script_command()
        .arg("--manifest-path")
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .expect("run scripts/platform-crates");
    assert!(
        !out.status.success(),
        "a workspace without `e2e` must fail rather than print a partial scope"
    );
}

/// The default output is what CI hands to `tst`: one `-p <name>` per line,
/// sorted, each name once, and every one a real package. `--explain` says
/// where each selection came from.
#[test]
fn platform_crates_output_is_sorted_unique_and_explained() {
    let f = Fixture::new()
        .with_crate("zeta", "#[cfg(unix)]\nfn a() {}\n#[cfg(unix)]\nfn b() {}\n")
        .with_crate("alpha", "\n\n#[cfg(windows)]\nfn a() {}\n");
    let out = f.run(&[]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, ["-p alpha", "-p e2e", "-p zeta"]);

    let out = f.run(&["--explain"]);
    assert!(out.status.success());
    let explain = String::from_utf8_lossy(&out.stdout);
    assert!(
        explain.contains("crates/alpha/src/lib.rs:3: cfg(windows)"),
        "{explain}"
    );
    assert!(explain.contains("e2e: always selected"), "{explain}");
}

/// Against the real tree: the crates that own most of the platform code are
/// in, `e2e` is always in, and the two crates with their own jobs are out.
#[test]
fn platform_crates_always_adds_e2e_and_never_go_or_bin() {
    let out = script_command()
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("run scripts/platform-crates");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    for present in ["-p e2e", "-p engine", "-p proc", "-p sandboxfuse"] {
        assert!(lines.contains(&present), "{present} missing from {lines:?}");
    }
    for absent in ["-p plugingo-e2e", "-p bin-e2e"] {
        assert!(!lines.contains(&absent), "{absent} present in {lines:?}");
    }
}
