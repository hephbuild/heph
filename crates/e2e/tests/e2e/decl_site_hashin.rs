//! I1 (error-reporting): nothing a diagnostic carries about where a target
//! was declared (`SpecSource`) reaches its `hashin`. Moving a target down a
//! line, or into a macro, leaves its `hashin` unchanged (targets that read
//! the edited files rebuild, as for any edit), for a target of every in-process
//! driver the engine registers, and for one a plugin function declares.
//!
//! The driver list is the engine's registry, not a list kept here: a driver
//! registered with no fixture fails the test, so a new driver gets one.
//! The engine is built as `heph` builds one (`commands::bootstrap`), plus the
//! go, oci, devenv and gh plugins in-process, with the parts their cdylibs
//! export. A cdylib driver is not run here: it never receives the source,
//! which the host leaves out of its `ParseRequest`
//! (`a_driver_parse_request_carries_no_spec_source` in `plugin-stabby`).

use futures::TryStreamExt as _;
use heph::commands::bootstrap;
use heph::engine::config_yaml;
use heph::engine::{ConfigYamlExt as _, Discovery, Engine, PluginParts};
use heph::htaddr::{Addr, parse_addr};
use hplugin_devenv::plugindevenv;
use hplugin_go::plugingo;
use hplugin_oci::pluginoci;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Each driver's fixture: the options of a `target()` of that driver besides
/// `name` and `driver`, one target that resolves to a def and a `hashin`
/// without running anything. A driver missing here fails the test.
fn fixture(driver: &str) -> Option<&'static str> {
    Some(match driver {
        "archive.create" => r#"deps = ["//base:txt"], out = "a.tar""#,
        "archive.extract" => r#"src = "//base:txt""#,
        "auth.credential" => concat!(
            r#"sources = [{"kind": "env", "names": ["TOKEN"]}], "#,
            r#"present = {"env": {"TOKEN": "${token}"}}"#
        ),
        "bash" => r#"run = "echo ok > $OUT", out = "o.txt""#,
        // `base/BUILD`, not the fixture's own: its content is what moves.
        "buildfile.format" | "buildfile.format_check" => r#"files = ["base/BUILD"]"#,
        "buildfile.lint_check" => concat!(
            r#"walk = '{"pkg": "base", "files": {"base/BUILD": {"pkg": "base", "loads": {}}}}', "#,
            r#"pluginFnsDigest = "0", driverKwargsDigest = "0""#
        ),
        "devenv.runner" => "",
        "exec" => r#"run = ["true"]"#,
        "fs" => r#"f = "base/BUILD""#,
        "gh.asset_url" => r#"src = "//base:txt", asset = "a.tgz", api = "https://api.github.com""#,
        "go.compile" => concat!(
            r#"p_flag = "x", out_file = "x.a", "#,
            r#"goos = "linux", goarch = "amd64", go_version = "1.22.0""#
        ),
        "go.golist" | "go.lint" => concat!(
            r#"import_path = "example.com/x", "#,
            r#"goos = "linux", goarch = "amd64", go_version = "1.22.0""#
        ),
        "go.toolchain" => concat!(
            r#"version = "1.22.0", goos = "linux", goarch = "amd64", "#,
            r#"sha256 = "0000000000000000000000000000000000000000000000000000000000000000""#
        ),
        "go.download" => r#"module = "example.com/m", version = "v1.0.0""#,
        "go.testmain" => r#"deps = {"golist": "//base:txt"}"#,
        "go.format" | "go.format_check" | "go.lint_fix" | "go.lint_gate" => "",
        "group" => r#"deps = ["//base:txt"]"#,
        "hostbin" => "",
        "http_fetch" => r#"url = "https://example.com/x""#,
        "nix" => {
            r#"nixpkgs = "0000000000000000000000000000000000000000", packages = ["hello"], programs = ["hello"]"#
        }
        "oci.builder_platform" => "",
        // Explicit platforms: with none, the def depends on asking the
        // builder for its default, which needs docker on the host.
        "oci.docker_build" => r#"platforms = ["linux/amd64"]"#,
        "oci.image" => r#"platforms = ["linux/amd64"], layers = ["//d_oci_layer:t"]"#,
        "oci.layer" => r#"srcs = ["//base:txt"], prefix = "/x""#,
        "oci.exists" | "oci.pull" => r#"ref = "example.com/x:1""#,
        "oci.index" => r#"images = ["//base:txt"]"#,
        "oci.load" | "oci.runner" => r#"image = "//base:txt""#,
        "oci.push" => r#"image = "//base:txt", ref = "example.com/x:1""#,
        "oci.tag" => r#"ref = "example.com/x:1", tags = ["a"]"#,
        "scratch" => "",
        "template" => r#"src = "//base:txt""#,
        "textfile" => r#"text = "hi""#,
        _ => return None,
    })
}

/// What fixtures that need a target to point at point at.
const BASE: &str = r#"target(name = "txt", driver = "textfile", text = "hi")
"#;

/// The engine the next `heph` invocation over `root` builds, with every
/// in-process plugin.
fn engine(root: &std::path::Path) -> Arc<Engine> {
    let file = config_yaml::load_from_root(root).expect("load .hephconfig");
    let config = file.resolve(root).expect("resolve config");
    let home = config.home_dir.to_path_buf();
    let mut e = Engine::new(config).expect("engine");
    bootstrap::register_builtin_plugins(&mut e, &home).expect("builtin plugins");
    bootstrap::register_builtin_factories(&mut e, &home).expect("builtin factories");
    e.register_plugins(&file.plugins).expect("plugins:");
    // With the provider each cdylib exports: it declares the targets a go and
    // an oci def depends on (`//@heph/go/gocache:…`, `//@heph/oci:…`), whose
    // addresses no option names. The drivers are the lists the cdylibs export.
    e.register_plugin("go", |init| {
        let go = plugingo::Provider::new(init.root.clone(), init.runtime.clone())?;
        let parts = PluginParts::default()
            .with_functions(go.functions())
            .with_provider(Box::new(go));
        Ok(plugingo::managed_drivers(None)
            .into_iter()
            .fold(parts, PluginParts::with_managed_driver))
    })
    .expect("go plugin");
    let oci_home = home.clone();
    e.register_plugin("oci", move |init| {
        let parts = PluginParts::default()
            .with_provider(Box::new(pluginoci::platform::Provider::new(&init.name)));
        Ok(pluginoci::managed_drivers(Ok(oci_home.clone()))
            .into_iter()
            .fold(parts, PluginParts::with_managed_driver))
    })
    .expect("oci plugin");
    let devenv_home = home.clone();
    e.register_plugin("devenv", move |_| {
        Ok(PluginParts::default().with_managed_driver(Box::new(
            plugindevenv::Driver::from_options(&Default::default(), &devenv_home)?,
        )))
    })
    .expect("devenv plugin");
    e.register_plugin("gh", |_| {
        Ok(PluginParts::default().with_managed_driver(Box::new(hplugin_gh::asset_url::Driver)))
    })
    .expect("gh plugin");
    let e = Arc::new(e);
    e.install_exec_runner_host();
    e
}

/// The package a driver's fixture lives in.
fn pkg(driver: &str) -> String {
    format!("d_{}", driver.replace(['.', '-'], "_"))
}

/// How a fixture's `target()` call is laid out in its package.
#[derive(Debug, Clone, Copy)]
enum Layout {
    /// The call alone, on line 1.
    Plain,
    /// The same call, three lines down.
    Moved,
    /// The call inside a macro in `defs/macros.bzl`, which the BUILD file
    /// calls.
    Macro,
}

const CONFIG: &str = "plugins:\n  - builtin: buildfile\n  - builtin: exec\n  - builtin: bash\n";

/// The matrix fixture's cell: a target a plugin function's callback declares.
const CELL: &str = r#"
def _cell(v):
    target(name = "m_" + v, driver = "bash", run = "echo " + v + " > $OUT", out = "o.txt")
    return ":m_" + v
"#;

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let abs = root.join(rel);
    std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
    std::fs::write(abs, content).expect("write");
}

/// Lays every fixture out as `layout` under `root`.
fn lay_out(root: &std::path::Path, drivers: &[(String, &str)], layout: Layout) {
    write(root, ".hephconfig", CONFIG);
    write(root, "base/BUILD", BASE);
    let mut macros = String::new();
    for (driver, opts) in drivers {
        let call = |name: &str| format!("target(name = {name}, driver = {driver:?}, {opts})");
        let build = match layout {
            Layout::Plain => format!("{}\n", call("\"t\"")),
            Layout::Moved => format!("# moved\n\n\n{}\n", call("\"t\"")),
            Layout::Macro => {
                let def = format!("mk_{}", pkg(driver));
                macros.push_str(&format!("def {def}(name):\n    {}\n\n", call("name")));
                format!("load(\"//defs/macros.bzl\", {def:?})\n\n{def}(name = \"t\")\n")
            }
        };
        write(root, &format!("{}/BUILD", pkg(driver)), &build);
    }
    let matrix = "M = heph.core.matrix(axes = {\"v\": [\"a\", \"b\"]}, cell = _cell)\n";
    let (matrix_build, matrix_macro) = match layout {
        Layout::Plain => (format!("{CELL}\n{matrix}"), String::new()),
        Layout::Moved => (format!("# moved\n\n\n{CELL}\n{matrix}"), String::new()),
        Layout::Macro => (
            "load(\"//defs/macros.bzl\", \"mk_matrix\")\n\nmk_matrix()\n".to_string(),
            format!("{CELL}\ndef mk_matrix():\n    {matrix}"),
        ),
    };
    macros.push_str(&matrix_macro);
    write(root, "matrix/BUILD", &matrix_build);
    write(root, "defs/macros.bzl", &macros);
    // `defs/` holds no BUILD file: the macros are only loaded.
}

/// Where a target's spec says it was declared: file, line and column.
type Declared = (String, u32, u32);

/// What `heph query //...` lists, and the `hashin` and declaration site of
/// each fixture target, on a fresh engine over `root`.
async fn observe(
    root: &std::path::Path,
    targets: &[Addr],
) -> (
    Vec<String>,
    BTreeMap<String, String>,
    BTreeMap<String, Declared>,
) {
    let engine = engine(root);
    let m = heph::htquery::parse("//...", &heph::htpkg::PkgBuf::from("")).expect("matcher");
    let mut listed: Vec<String> = Arc::clone(&engine)
        .query(engine.new_state(), &m, Discovery::Complete)
        .map_ok(|a| a.format())
        .try_collect()
        .await
        .expect("query //...");
    listed.sort();
    let mut hashin = BTreeMap::new();
    let mut declared = BTreeMap::new();
    let mut failed = Vec::new();
    for addr in targets {
        // The test's precondition: each spec carries a source, or a driver
        // that hashed it would hash `None` in every layout and pass.
        match Arc::clone(&engine).get_spec(engine.new_state(), addr).await {
            Ok(spec) => match spec.source.as_ref() {
                Some(source) => {
                    let call = source.call();
                    declared.insert(
                        addr.format(),
                        (source.file().to_owned(), call.start_line, call.start_col),
                    );
                }
                None => failed.push(format!("{}: the spec carries no source", addr.format())),
            },
            Err(e) => failed.push(format!("{}: spec: {e:#}", addr.format())),
        }
        match Arc::clone(&engine).meta(engine.new_state(), addr).await {
            Ok(meta) => {
                hashin.insert(addr.format(), meta.hashin.clone());
            }
            Err(e) => failed.push(format!("{}: {e:#}", addr.format())),
        }
    }
    assert!(
        failed.is_empty(),
        "a fixture with no source or no hashin (fix it in `fixture`):\n{}",
        failed.join("\n")
    );
    (listed, hashin, declared)
}

#[tokio::test]
async fn moved_target_same_hashin_every_registered_driver() {
    let dir = hcore::fsutil::tempdir().expect("tempdir");
    let root = dir.path();
    write(root, ".hephconfig", CONFIG);
    let registered = engine(root).driver_names();
    let missing: Vec<&String> = registered.iter().filter(|d| fixture(d).is_none()).collect();
    assert!(
        missing.is_empty(),
        "registered drivers with no fixture in `fixture`: {missing:?}"
    );
    let drivers: Vec<(String, &str)> = registered
        .iter()
        .filter_map(|d| Some((d.clone(), fixture(d)?)))
        .collect();
    let mut targets: Vec<Addr> = drivers
        .iter()
        .map(|(d, _)| parse_addr(&format!("//{}:t", pkg(d))).expect("addr"))
        .collect();
    targets.extend(["//matrix:m_a", "//matrix:m_b"].map(|a| parse_addr(a).expect("addr")));

    lay_out(root, &drivers, Layout::Plain);
    let (listed, plain, plain_at) = observe(root, &targets).await;
    for layout in [Layout::Moved, Layout::Macro] {
        lay_out(root, &drivers, layout);
        let (l, h, at) = observe(root, &targets).await;
        assert_eq!(
            l, listed,
            "{layout:?}: `heph query //...` lists the same targets"
        );
        for (addr, want) in &plain {
            assert_ne!(
                at.get(addr),
                plain_at.get(addr),
                "{layout:?}: {addr} was not moved, so its hashin proves nothing"
            );
            assert_eq!(h.get(addr), Some(want), "{layout:?}: {addr}'s hashin moved");
        }
    }
}
