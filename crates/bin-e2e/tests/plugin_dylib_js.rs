//! The js/ts cdylib plugin, against the artifact CI actually publishes.
//!
//! Split out from `plugin_dylib.rs` (which covers the go and gha cdylibs)
//! rather than grown into it: proving the js provider needs real workspace
//! fixtures (a `pnpm-workspace.yaml` plus one or more `package.json` files),
//! not just a one-line manifest + option, so keeping it in its own file
//! keeps that fixture-building code from crowding out the smaller go/gha
//! tests. Same rationale as `plugin_dylib.rs`'s own module doc: in-process
//! tests construct `plugin-js` directly and never cross the dynamic seam —
//! a symbol missing from the built `.so`/`.dylib`, an ABI version the host
//! refuses, or the manifest → checksum → `dlopen` chain are all invisible to
//! them.
//!
//! ## Why this file has no `functions()`-based test
//!
//! `plugin_dylib.rs`'s go test proves a real ABI round trip by asserting a
//! specific provider function (`build_addr`) comes back from
//! `inspect functions`. The js provider has no equivalent: unlike
//! `plugin-go`'s `Provider`, `plugin-js`'s `Provider` impl never overrides
//! `Provider::functions` (see `crates/plugin-js/src/pluginjs/provider.rs`'s
//! `impl ProviderTrait for Provider` block), so it falls through to the
//! trait's own default — an empty `vec![]` (`crates/plugin/src/provider.rs`,
//! `fn functions(&self) -> Vec<ProviderFunctionDef> { vec![] }`). There is no
//! function name to assert on. [`shipped_js_cdylib_loads_and_answers_across_the_abi`]
//! below still exercises `inspect functions` (to prove that call answers
//! cleanly across the seam rather than erroring), but the tests that actually
//! prove the provider returns *real, specific* data are
//! [`shipped_js_cdylib_discovers_a_real_workspace_package`], which drives a
//! real `Provider::list_packages`/`Provider::list` round trip and asserts a
//! specific target address comes back — the js-provider analog of
//! `shipped_gha_cdylib_loads`'s `query` check, not the go test's
//! `functions`-only one — and
//! [`shipped_js_cdylib_resolves_pnpm_workspace_glob_membership`], which
//! covers a seam that discovery alone does not: `Provider::list`/`list_packages`
//! find a package by `package.json` presence alone
//! (`collect_js_packages`), independent of `pnpm-workspace.yaml` entirely, so
//! a discovery-only test proves nothing about pnpm's `packages` glob. Only
//! `Provider::get` (via `deps_config` → `member_addrs_by_name` →
//! `workspace::read_pnpm_workspace_globs`/`resolve_members`) actually
//! consults the glob, so that test resolves a `js_package_info` target
//! instead of just listing/querying.

mod common;

use common::{BASE_CONFIG, Dist, Workspace, describe, sha256_file, write_manifest};

/// A shipped cdylib must load *and answer*. `pkgmanager` is the js
/// provider's one required option (mirrors the go provider's `gotool`) —
/// there is no implicit default (see `Provider::from_options` in
/// `crates/plugin-js/src/pluginjs/provider.rs`: a repo with both a
/// `pnpm-workspace.yaml` and a `package.json` `"workspaces"` array would
/// otherwise have an ambiguous, silently-picked answer). Setting it and
/// running `inspect functions` proves `heph_plugin_create` (construction,
/// including the required-option check) and the `functions()` sync ABI
/// call both succeed cleanly — a real round trip, even though the answer
/// itself is an empty list (see this file's module doc for why there is no
/// function name to assert on here). An empty *and clean* result still
/// rules out a real failure mode: a corrupt/incompatible response decodes
/// to the same empty `Vec` (`crates/plugin-stabby/src/load_stable.rs`'s
/// `functions()` swallows a decode error into `Vec::new()` deliberately, to
/// avoid poisoning registry wiring that has no error channel) — the command
/// exiting non-zero, or printing anything at all, is what would catch that.
#[test]
fn shipped_js_cdylib_loads_and_answers_across_the_abi() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("js");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-js-plugin.json");
    let sum = sha256_file(&dylib).expect("hash js cdylib");
    write_manifest(&manifest, "js", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      pkgmanager: \"npm\"\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    // NOT an empty-stdout check: `inspect functions` always lists the `fs.*`
    // functions too — `fs` (`crates/builtins/src/pluginfs`) is registered
    // unconditionally by `bootstrap::new_engine`, independent of the
    // `plugins:` config, so its lines are present in every run regardless of
    // whether the js plugin is loaded at all. Each line is rendered as
    // `<provider name>.<function>` (`Engine::provider_functions`), and the
    // js provider registers under the name `"js"` (`Provider::config` in
    // `crates/plugin-js/src/pluginjs/provider.rs`) — a name the engine
    // enforces as unique across providers, so no other provider can produce
    // a `js.`-prefixed line. The specific, provable claim is: nothing under
    // that prefix came back, which is what `functions()` returning `vec![]`
    // actually means at this seam.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.lines().any(|line| line.starts_with("js.")),
        "js provider declares no BUILD-file functions, but a `js.`-prefixed \
         line came back across the seam: {}",
        describe(&out)
    );
}

/// The real discovery/query round trip: proves `Provider::list_packages`
/// and `Provider::list` answer with genuine data across the ABI seam,
/// against a synthetic workspace with one discoverable package. No
/// `dependencies`/`devDependencies` are declared, so nothing here ever needs
/// `js_install`'s network fetch (no npm/pnpm registry access, no host
/// toolchain) — this test is about the loader and the provider's own
/// package discovery, not about installing anything.
///
/// The `pnpm-workspace.yaml` written below is *not* exercising pnpm's
/// `packages`-glob membership convention — `collect_js_packages` (what
/// `Provider::list`/`list_packages` actually call) discovers a package by
/// `package.json` presence alone, independent of the glob or even of
/// `pkgmanager`. It's here only because `query -e //...` still needs a
/// well-formed workspace to walk; `pkgmanager: "pnpm"` would work identically
/// with the file absent. See
/// [`shipped_js_cdylib_resolves_pnpm_workspace_glob_membership`] below for
/// the test that actually exercises the glob.
///
/// Mirrors `shipped_gha_cdylib_loads`'s shape (a `query` resolving the
/// workspace graph proves the plugin is really wired in), not the go test's
/// `functions`-only check — see this file's module doc for why.
#[test]
fn shipped_js_cdylib_discovers_a_real_workspace_package() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("js");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-js-plugin.json");
    let sum = sha256_file(&dylib).expect("hash js cdylib");
    write_manifest(&manifest, "js", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      pkgmanager: \"pnpm\"\n",
        manifest.display()
    ))
    .expect("write config");

    // Not exercising the glob (see this test's doc) — present only so the
    // workspace is well-formed for `pkgmanager: "pnpm"`.
    ws.write("pnpm-workspace.yaml", "packages:\n  - packages/*\n")
        .expect("write pnpm-workspace.yaml");
    // `name` is required — `workspace::read_package_name` errors on a
    // package.json without one — and no `dependencies`/`main`/test files
    // are declared, so `Provider::list` never needs to resolve an import,
    // fetch a dependency, or run a toolchain to answer this query.
    ws.write("packages/foo/package.json", "{\"name\": \"foo\"}\n")
        .expect("write package.json");

    // A query resolving the workspace graph means the js provider loaded,
    // was handed `pkgmanager`, walked the tree, and found the package —
    // real data, not just a process exit code. `-e` is required for subtree
    // selection: a bare positional argument parses strictly as one address.
    let out = ws.run(&dist, &["query", "-e", "//..."]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        // `PACKAGE_INFO_TARGET` in `crates/plugin-js/src/pluginjs/mod.rs`
        // ("package_info") — the one target `Provider::list` always emits
        // for a discovered package.
        stdout.contains("//packages/foo:package_info"),
        "js provider's discovered package did not come back across the seam: {}",
        describe(&out)
    );
}

/// Proves pnpm's own workspace-member convention — the `packages` glob list
/// in `pnpm-workspace.yaml` — is actually consulted across the ABI seam, not
/// just parsed in-process. `Provider::list`/`list_packages` (exercised by
/// [`shipped_js_cdylib_discovers_a_real_workspace_package`] above) discover a
/// package by `package.json` presence alone
/// (`provider.rs::collect_js_packages`), completely independent of the glob
/// — so that test cannot catch a broken
/// `workspace.rs::read_pnpm_workspace_globs`/`resolve_members`. Only
/// `Provider::get`'s dependency resolution consults it, via
/// `deps_config` → `member_addrs_by_name` →
/// `workspace::resolve_members` (see `provider.rs::member_addrs_by_name_blocking`).
///
/// The workspace has two packages with real `package.json` files on disk:
/// `packages/foo` (inside the `packages/*` glob, so a workspace member) and
/// `outside/bar` (deliberately *outside* the glob, so `collect_js_packages`
/// still finds it during a filesystem walk, but `resolve_members` must not
/// admit it as a member). `foo` declares a required dependency on `bar` by
/// name. If glob filtering worked, `member_addrs_by_name` would have no
/// entry for `bar` — `resolve_one_dependency` falls through to the (absent)
/// lockfile, finds no resolution, and — `bar` not being optional — hard-fails
/// `Provider::get` for `//packages/foo:package_info` with a specific,
/// recognizable message. If glob filtering were broken (e.g. `resolve_members`
/// admitted every discovered package regardless of the glob, the exact bug
/// this test exists to catch), `bar` would resolve straight to its own
/// `package_info` addr by name and the command would exit 0 instead.
#[test]
fn shipped_js_cdylib_resolves_pnpm_workspace_glob_membership() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("js");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-js-plugin.json");
    let sum = sha256_file(&dylib).expect("hash js cdylib");
    write_manifest(&manifest, "js", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      pkgmanager: \"pnpm\"\n",
        manifest.display()
    ))
    .expect("write config");

    // Only `packages/*` is a member glob — `outside/*` is deliberately not.
    ws.write("pnpm-workspace.yaml", "packages:\n  - packages/*\n")
        .expect("write pnpm-workspace.yaml");
    // A required (non-optional) dependency on `bar` by name — the only way
    // `member_addrs_by_name` gets consulted for a specific name.
    ws.write(
        "packages/foo/package.json",
        "{\"name\": \"foo\", \"dependencies\": {\"bar\": \"1.0.0\"}}\n",
    )
    .expect("write packages/foo/package.json");
    // A real package.json on disk, discoverable by `collect_js_packages`'s
    // plain filesystem walk — but outside the `packages/*` glob, so it must
    // not be treated as a workspace member.
    ws.write("outside/bar/package.json", "{\"name\": \"bar\"}\n")
        .expect("write outside/bar/package.json");

    let out = ws
        .run(&dist, &["inspect", "def", "//packages/foo:package_info"])
        .expect("run");
    assert!(
        !out.status.success(),
        "`bar` should not have resolved as a workspace member — glob \
         filtering appears to have been skipped: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bar") && stderr.contains("no lockfile resolution"),
        "expected the specific unresolved-required-dependency error for \
         `bar` (proving it was never admitted as a workspace member): {}",
        describe(&out)
    );
}

/// Regression test for the same log-sink-before-`create` ordering bug
/// `plugin_dylib.rs`'s go-flavored `plugin_construction_failure_logs_before_the_abort`
/// covers: if the host installs its log sink *after* calling the plugin's
/// `create`, a `tracing::error!` logged during construction failure has no
/// subscriber to go to and is silently dropped — right before the ABI seam
/// turns the failure into a non-unwinding abort with zero diagnostic
/// output. `crates/plugin-js-cdylib/src/lib.rs`'s `heph_plugin_create` has
/// the identical shape (`tracing::error!(...)` then
/// `std::process::abort()` on a `build()` error), so this is real coverage
/// of the js cdylib, not a duplicate of the go one — a bug specific to how
/// this cdylib wires its log sink would not be caught by testing go's.
///
/// The shipped js plugin fails construction deterministically when
/// `pkgmanager` is omitted — it is required, with no implicit default (see
/// `shipped_js_cdylib_loads_and_answers_across_the_abi`'s doc) — no
/// purpose-built test hook needed.
#[test]
fn js_plugin_construction_failure_logs_before_the_abort() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("js");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-js-plugin.json");
    let sum = sha256_file(&dylib).expect("hash js cdylib");
    write_manifest(&manifest, "js", &dylib, Some(&sum)).expect("write manifest");

    // No `options:` at all — `pkgmanager` is required, so this must fail
    // construction rather than fall back to some default.
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(
        !out.status.success(),
        "plugin construction should have failed and aborted: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plugin construction failed") && stderr.contains("pkgmanager"),
        "construction-failure log was dropped — the log sink must be installed \
         before `create` is called: {}",
        describe(&out)
    );
}
