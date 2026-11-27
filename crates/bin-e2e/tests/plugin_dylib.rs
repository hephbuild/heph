//! The cdylib plugin loader, against the cdylibs CI actually publishes.
//!
//! In-process tests construct `plugin-go` / `plugin-gh` directly and call
//! them through Rust generics — the dynamic seam is never crossed. Everything
//! that can only break at the seam is invisible to them: a symbol missing from
//! the built `.so`/`.dylib`, an ABI version the host refuses, a plugin whose
//! own statically-linked tokio/tracing misbehaves once loaded into a foreign
//! process, and the manifest → checksum → `dlopen` resolution chain.

mod common;

use std::process::{Output, Stdio};

use common::{
    BASE_CONFIG, Dist, RUN_DEADLINE, Workspace, describe, sha256_file, wait_output, write_manifest,
};

/// A shipped cdylib must load *and answer*. Merely dlopening proves little —
/// `inspect functions` renders the signatures the loaded plugin's function
/// metadata carried across the ABI, so a passing assertion means a real
/// round trip across the ABI seam with real data coming back.
#[test]
fn shipped_go_cdylib_loads_and_answers_across_the_abi() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    // `gotool: host` keeps the fixture offline: toolchain resolution is lazy, so
    // nothing is downloaded and no `go` is executed unless a Go target is built
    // — and none is. This test is about the loader, not about Go.
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("build_addr"),
        "go provider's `build_addr` did not come back across the seam: {}",
        describe(&out)
    );
}

/// **A driver inside a cdylib runs its target in the environment a runner
/// describes.**
///
/// This is the seam nothing else covers, and it shipped broken because of that.
/// A cdylib links its own copy of `execrunner`, whose host registry the engine
/// populated only in the *binary* — so a plugin driver under a runner failed
/// with "no runner host is installed in this component" no matter how correct
/// the engine side was. Every in-process test passed: `plugingo-e2e` links
/// `plugin-go` directly, and `exec_runner.rs` drives the builtin bash/exec
/// drivers. Neither crosses a real `dlopen`.
///
/// The proof does not need Go to succeed, or even to exist. `gotool = "host"`
/// resolves the toolchain by spawning a bare `go` **through the runner**, so a
/// `wrap` prefix that announces itself on stderr is evidence that a driver
/// living in the cdylib reached the host's registry across the ABI. Whatever
/// the Go build then does is beside the point.
#[test]
fn a_cdylib_driver_runs_its_target_through_the_hosts_runner() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    // The go plugin's `runner:` option is the only way a *generated* Go target
    // ever names a runner, so it is also the only way to reach this seam.
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n      runner: \"//:runner\"\n",
        manifest.display()
    ))
    .expect("write config");

    // A `wrap` runner written by hand, which is the point of `runner.json` being
    // a plain artifact: no plugin is needed to produce one, so this test needs
    // neither devenv nor docker.
    //
    // The prefix is a program that deliberately does not exist. A `wrap` runner
    // makes its prefix head the program, so the driver's `go env GOROOT` probe
    // tries to exec *this* — and `hexecrunner::output` names what a runner
    // substituted, so the marker lands in the failure. Only reachable if the
    // rewrite crossed back from the host.
    //
    // No shell and no quoting on purpose: Starlark strips backslash escapes
    // inside a triple-quoted string, which silently turns an `exec "$@"` prefix
    // into malformed JSON.
    let runner_json = r#"{
  "version": 1,
  "fingerprint": "test:cdylib-seam",
  "runner": "wrap",
  "config": { "prefix": ["/heph-runner-reached-the-host"] }
}
"#;
    let (goos, goarch) = common::host_os_arch();
    // Both at the workspace root: `provider_state` applies to its own package and
    // descendants, and the Go package is `cmd/`, so a state in `pkg/` would not
    // reach it — the variant lookup walks *ancestors* within the module.
    ws.write(
        "BUILD",
        &format!(
            r#"target(
    name = "runner",
    driver = "textfile",
    text = """{runner_json}""",
    out = "runner.json",
)
provider_state(
    provider = "go",
    variants = {{"host": {{"goos": "{goos}", "goarch": "{goarch}"}}}},
)
"#
        ),
    )
    .expect("write BUILD");
    ws.write("go.mod", "module example.com/seam\n\ngo 1.21\n")
        .expect("write go.mod");
    ws.write("cmd/main.go", "package main\n\nfunc main() {}\n")
        .expect("write main.go");

    let out = ws.run(&dist, &["run", "//cmd:build@v=host"]).expect("run");
    let combined = describe(&out);

    // The failure this test exists for. Named separately so a regression reports
    // the cause rather than "the marker was missing".
    assert!(
        !combined.contains("no runner host is installed"),
        "the cdylib was not handed the host's runner registry — \
         `heph_plugin_set_runner_host` is the wiring that does it: {combined}"
    );
    // Only reachable if the rewrite crossed back: the driver in the cdylib built
    // a spec naming `go`, and what it actually tried to spawn was the host wrap
    // runner's prefix.
    assert!(
        combined.contains("/heph-runner-reached-the-host"),
        "a driver in the cdylib must spawn through the host's wrap runner: {combined}"
    );
}

/// **A cdylib driver can start a cold `session` runner.**
///
/// Plugins load on rayon workers, where there is no current tokio runtime. A
/// loader that probed `Handle::try_current()` there always got `Err` and handed
/// the plugin an *inline* runner host, so the host's `prepare` body ran on the
/// plugin worker that polled it. A `wrap` runner never notices (its `prepare`
/// touches no timer or IO), but a cold `session` start polls for the agent's
/// socket with a host `tokio::time::sleep` — off the host runtime that is a
/// panic, and a panic at the ABI seam is a non-unwinding abort. (The inline
/// loader passed this on darwin/arm64: the polling thread there happened to
/// carry the host runtime. The loader now passes the engine's handle so the
/// outcome no longer depends on which thread polls.)
///
/// Nothing in-process uses the runner first, so the session is genuinely cold
/// when the cdylib driver reaches it. The launch is a passthrough shell that
/// drops a marker, so the marker proves the session start ran to the spawn.
#[test]
fn a_cdylib_driver_starts_a_cold_session_runner() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n      runner: \"//:runner\"\n",
        manifest.display()
    ))
    .expect("write config");

    // `sh -c SCRIPT ARG0 ARGS…`: the runner appends the agent invocation, so the
    // heph binary lands in `$0` and its arguments in `$@`. Unquoted on purpose:
    // Starlark strips backslash escapes in a triple-quoted string, and the
    // temp paths involved carry no spaces.
    let marker = ws.root().join("session-launched");
    let runner_json = format!(
        r#"{{
  "version": 1,
  "fingerprint": "test:cold-session",
  "runner": "session",
  "config": {{ "launch": ["/bin/sh", "-c", "touch {} && exec $0 $@"] }}
}}
"#,
        marker.display()
    );
    let (goos, goarch) = common::host_os_arch();
    ws.write(
        "BUILD",
        &format!(
            r#"target(
    name = "runner",
    driver = "textfile",
    text = """{runner_json}""",
    out = "runner.json",
)
provider_state(
    provider = "go",
    variants = {{"host": {{"goos": "{goos}", "goarch": "{goarch}"}}}},
)
"#
        ),
    )
    .expect("write BUILD");
    ws.write("go.mod", "module example.com/seam\n\ngo 1.21\n")
        .expect("write go.mod");
    ws.write("cmd/main.go", "package main\n\nfunc main() {}\n")
        .expect("write main.go");

    // Whether Go is on the host is beside the point: the toolchain probe goes
    // through the runner either way, and that is the cold start under test.
    let out = ws.run(&dist, &["run", "//cmd:build@v=host"]).expect("run");
    let combined = describe(&out);

    assert!(
        out.status.code().is_some(),
        "heph died on a signal (an abort at the plugin seam): {combined}"
    );
    assert!(
        !combined.contains("there is no reactor running"),
        "the host's runner `prepare` ran off the host runtime: {combined}"
    );
    assert!(
        !combined.contains("panic in a function that cannot unwind"),
        "a panic crossed the plugin seam: {combined}"
    );
    assert!(
        marker.is_file(),
        "the cold session runner never launched: {combined}"
    );
}

/// (Re)write `ws`'s config to enable the shipped `gh` cdylib with `options`
/// (YAML lines already indented under the entry's `options:`, or empty).
fn configure_gh(ws: &Workspace, dist: &Dist, options: &str) {
    let dylib = dist.plugin("gh");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-gh-plugin.json");
    let sum = sha256_file(&dylib).expect("hash gh cdylib");
    write_manifest(&manifest, "gh", &dylib, Some(&sum)).expect("write manifest");

    let options = if options.is_empty() {
        String::new()
    } else {
        format!("    options:\n{options}")
    };
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n{options}",
        manifest.display()
    ))
    .expect("write config");
}

/// The `options:` lines for the gh plugin: `top` (top-level lines, already
/// indented, or empty) and, `hook_enabled`, a `hook:` map turning the hook on.
/// Disabled is no `hook:` at all, the default.
fn gh_options(top: &str, hook_enabled: bool) -> String {
    if hook_enabled {
        format!("{top}      hook:\n        enable: true\n")
    } else {
        top.to_string()
    }
}

/// `heph <args>` in a controlled GitHub Actions environment: every
/// `GITHUB_*`/`RUNNER_*` variable of the test's own environment is removed (on
/// a CI runner they would point the hook at the real job's summary and token),
/// then `GITHUB_ACTIONS=true` with [`step_summary`] as the step summary. Set
/// on the child only, never on this process. Whether the hook exists is the
/// config's `hook.enable`, never this environment.
fn run_gh(ws: &Workspace, dist: &Dist, args: &[&str]) -> Output {
    let mut cmd = ws.cmd(dist, args);
    for (key, _) in std::env::vars_os() {
        let actions = key
            .to_str()
            .is_some_and(|k| k.starts_with("GITHUB_") || k.starts_with("RUNNER_"));
        if actions {
            cmd.env_remove(&key);
        }
    }
    cmd.env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", step_summary(ws));
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heph");
    wait_output(child, RUN_DEADLINE).expect("heph ran")
}

/// The `$GITHUB_STEP_SUMMARY` [`run_gh`] points the hook at.
fn step_summary(ws: &Workspace) -> std::path::PathBuf {
    ws.root().join("step-summary.md")
}

/// The driver of `addr`'s spec, from `heph inspect spec` (which does not build).
fn spec_driver(ws: &Workspace, dist: &Dist, addr: &str) -> String {
    let out = run_gh(ws, dist, &["inspect", "spec", addr]);
    assert!(out.status.success(), "{addr}: {}", describe(&out));
    let driver = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|out| Some(out.get("spec")?.get("driver")?.as_str()?.to_string()));
    assert!(
        driver.is_some(),
        "{addr}: no driver in the spec: {}",
        describe(&out)
    );
    driver.unwrap_or_default()
}

/// The second shipped cdylib, exporting a hook, a function and drivers rather
/// than a provider — different export kinds over the same seam, so a loader
/// that only handles providers fails here and nowhere else.
///
/// `heph.gh.release_asset` is called across the seam and its declarations come
/// back: `inspect spec` resolves the declared targets without building, so no
/// network is needed. The extract it declares is the host's builtin
/// `archive.extract`, not the cdylib's, and the release binary must have it:
/// it unpacks an archive built offline by the builtin coreutils' `tar`, with
/// the gh cdylib loaded. Both with the hook
/// enabled (`hook: {enable: true}`, when it writes the step summary at the end
/// of the run) and without (when it writes nothing, though `GITHUB_ACTIONS`
/// is `true`).
#[test]
fn shipped_gh_cdylib_loads() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");
    ws.write(
        "rel/BUILD",
        r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "t.tar.gz",
    extract = True,
)
"#,
    )
    .expect("write BUILD");
    let marker = ws.root().join("hello.txt");
    ws.write(
        "ex/BUILD",
        &format!(
            r#"
target(
    name = "archive",
    driver = "bash",
    run = "mkdir -p d/bin && printf 'hello from the archive\n' > d/bin/hello && chmod +x d/bin/hello && tar -czf t.tar.gz -C d bin",
    out = "t.tar.gz",
)

target(
    name = "unpacked",
    driver = "archive.extract",
    src = ":archive",
    format = "tar.gz",
)

target(
    name = "check",
    driver = "bash",
    deps = [":unpacked"],
    run = "test -x unpacked/bin/hello && cat unpacked/bin/hello > {marker}",
    out = [],
    cache = False,
)
"#,
            marker = marker.display()
        ),
    )
    .expect("write BUILD");

    for hook in [false, true] {
        configure_gh(&ws, &dist, &gh_options("", hook));
        // A query still resolving the workspace graph means the plugin (and,
        // enabled, its hook) registered and the engine came up with it
        // attached. Subtree selection needs `-e`: a bare positional argument
        // is parsed strictly as an address.
        let out = run_gh(&ws, &dist, &["query", "-e", "//pkg/..."]);
        assert!(out.status.success(), "{}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("//pkg:ok"),
            "{}",
            describe(&out)
        );

        assert_eq!(
            spec_driver(&ws, &dist, "//rel:tool"),
            "archive.extract",
            "hook={hook}"
        );
        assert_eq!(
            spec_driver(&ws, &dist, "//rel:tool_archive"),
            "http_fetch",
            "hook={hook}"
        );

        for stale in [&marker, &step_summary(&ws)] {
            if stale.exists() {
                std::fs::remove_file(stale).expect("remove the previous run's file");
            }
        }
        let out = run_gh(&ws, &dist, &["run", "//ex:check"]);
        assert!(out.status.success(), "{}", describe(&out));
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap_or_default(),
            "hello from the archive\n",
            "hook={hook}: {}",
            describe(&out)
        );
        // `hook.enable` alone decides whether the hook reports, across the
        // seam: `GITHUB_ACTIONS=true` on its own builds none.
        assert_eq!(
            step_summary(&ws).is_file(),
            hook,
            "hook={hook}: {}",
            describe(&out)
        );
    }
}

/// A misused `release_asset` call fails the command with the function's
/// message, as an exit code: the error crosses the seam as a value, not as a
/// panic that would abort the host.
#[test]
fn shipped_gh_cdylib_reports_a_misused_call() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "bad/BUILD",
        r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "t.tar.gz",
    extract = True,
    executable = True,
)
"#,
    )
    .expect("write BUILD");

    for hook in [false, true] {
        configure_gh(&ws, &dist, &gh_options("", hook));
        let out = run_gh(&ws, &dist, &["inspect", "spec", "//bad:tool"]);
        assert!(
            out.status.code().is_some(),
            "heph died on a signal: {}",
            describe(&out)
        );
        assert!(!out.status.success(), "{}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("`executable` marks a fetched file"),
            "hook={hook}: {}",
            describe(&out)
        );
    }
}

/// With `credentials`, `release_asset` declares the API lookup, the
/// `gh.asset_url` driver that reads it, and the asset fetch, all across the
/// seam. Resolved by `inspect spec`, so neither the token nor the network is
/// touched.
#[test]
fn shipped_gh_cdylib_declares_a_private_asset() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "priv/BUILD",
        r#"
target(
    name    = "github",
    driver  = "auth.credential",
    sources = [heph.auth.env(["GITHUB_TOKEN"])],
    present = heph.auth.netrc(["api.github.com"],
                              login = "x-access-token", password = "${github_token}"),
)

heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "t.tar.gz",
    extract = True,
    credentials = [":github"],
)
"#,
    )
    .expect("write BUILD");

    for hook in [false, true] {
        configure_gh(&ws, &dist, &gh_options("", hook));
        for (addr, driver) in [
            ("//priv:tool", "archive.extract"),
            ("//priv:tool_archive", "http_fetch"),
            ("//priv:tool_url", "gh.asset_url"),
            ("//priv:tool_release", "http_fetch"),
        ] {
            assert_eq!(spec_driver(&ws, &dist, addr), driver, "{addr}, hook={hook}");
        }
    }
}

/// A bad plugin option does not abort the host at load: the log sink is not up
/// when the cdylib is created, so an abort there would be a bare `SIGABRT`.
/// The plugin loads, the build resolves, and the hook that stands in for the
/// one that could not be configured logs the option as an unknown key once a
/// build sends it an event. A
/// `serverUrl` (the server is `release_asset`'s `server_url` argument, not an
/// option), with the hook on and off, and a hook option left at the top level.
/// `release_asset` reads no option, so it still declares its targets; a bad
/// `server_url` argument fails that call, naming the argument.
#[test]
fn shipped_gh_cdylib_survives_a_bad_option() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");
    ws.write(
        "rel/BUILD",
        r#"
heph.gh.release_asset(name = "tool", repo = "o/r", tag = "v1", asset = "t.tar.gz")
"#,
    )
    .expect("write BUILD");
    ws.write(
        "badserver/BUILD",
        r#"
heph.gh.release_asset(
    name = "tool",
    repo = "o/r",
    tag = "v1",
    asset = "t.tar.gz",
    server_url = "github.com",
)
"#,
    )
    .expect("write BUILD");

    let server = "      serverUrl: \"https://ghe.example\"\n";
    for (options, wants) in [
        (gh_options(server, false), "unknown field `serverUrl`"),
        (gh_options(server, true), "unknown field `serverUrl`"),
        (
            "      refreshSecs: 30\n".to_string(),
            "unknown field `refreshSecs`",
        ),
    ] {
        configure_gh(&ws, &dist, &options);
        let out = run_gh(&ws, &dist, &["query", "-e", "//pkg/..."]);
        assert!(
            out.status.code().is_some(),
            "heph died on a signal: {}",
            describe(&out)
        );
        assert!(out.status.success(), "{}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("//pkg:ok"),
            "{}",
            describe(&out)
        );

        // The stand-in hook logs on the build's first event, so it is a build
        // (not a query, which sends hooks none) that names the bad key.
        let out = run_gh(&ws, &dist, &["run", "//pkg:ok"]);
        assert!(
            out.status.code().is_some(),
            "heph died on a signal: {}",
            describe(&out)
        );
        assert!(out.status.success(), "{}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(wants),
            "{options}: {}",
            describe(&out)
        );

        assert_eq!(
            spec_driver(&ws, &dist, "//rel:tool"),
            "http_fetch",
            "{options}"
        );

        let out = run_gh(&ws, &dist, &["inspect", "spec", "//badserver:tool"]);
        assert!(
            out.status.code().is_some(),
            "heph died on a signal: {}",
            describe(&out)
        );
        assert!(!out.status.success(), "{}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("`server_url` \"github.com\""),
            "{options}: {}",
            describe(&out)
        );
    }
}

/// The spawn-at-the-seam host mirror, across a REAL cdylib: the go provider's
/// `list` calls back into the engine executor (`states_under`, the module
/// variant universe) from a plugin-runtime worker. In-process tests
/// structurally cannot cover this — host and plugin share one tokio image
/// there, so the callback body always finds a runtime to run on. Here the
/// callback future is built by the host binary and polled by the plugin's own
/// workers: exactly the asymmetry that panics ("no reactor running") and then
/// aborts at the extern seam if the host side does not spawn the body onto the
/// engine runtime.
///
/// The fixture mirrors plugingo-e2e's `variant_sibling`: the `release` variant
/// is declared ONLY at `//cmd`, so it is absent from `//lib`'s ancestry and
/// `//lib:build_lib@v=release` can be listed only if the module universe came
/// back through the `states_under` callback — the assertion cannot pass
/// vacuously. `gotool: host` + query-only keeps the fixture offline: listing
/// runs no toolchain.
#[test]
fn shipped_go_cdylib_list_calls_back_states_under_across_the_seam() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");

    ws.write("go.mod", "module example.com/seam\n\ngo 1.21\n")
        .expect("write go.mod");
    ws.write(
        "lib/lib.go",
        "package lib\n\nfunc Greet() string { return \"hi\" }\n",
    )
    .expect("write lib.go");
    ws.write("cmd/main.go", "package main\n\nfunc main() {}\n")
        .expect("write main.go");
    ws.write(
        "cmd/BUILD",
        "provider_state(\n    provider = \"go\",\n    variants = {\"release\": {\"goos\": \"linux\", \"goarch\": \"amd64\"}},\n)\n",
    )
    .expect("write BUILD");

    // `--candidates`: the listing is what crosses the seam, and confirming a
    // match would run `go list`.
    let out = ws
        .run(&dist, &["query", "--candidates", "-e", "//lib/..."])
        .expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("//lib:build_lib@v=release"),
        "the sibling-declared variant must come back through the plugin's \
         states_under callback across the seam: {}",
        describe(&out)
    );
}

/// Listed facts cross the REAL cdylib seam: the go plugin's `list` carries
/// each entry's driver, and the host decides `driver(...)` from it without a
/// single `get`. `get` of any go target runs `_golist`, which needs `go`; the
/// only `go` on `PATH` here is a stub that fails, so a run that resolved even
/// one spec would fail. `driver("nonexistent")` must therefore come back
/// empty and green (a listed No is never resolved), and
/// `--candidates` with `driver(go.compile)` must name the lib's compile —
/// both decided from the listing alone.
#[cfg(unix)]
#[test]
fn shipped_go_cdylib_listed_facts_decide_without_get() {
    use std::os::unix::fs::PermissionsExt;

    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");
    ws.write("go.mod", "module example.com/facts\n\ngo 1.21\n")
        .expect("write go.mod");
    ws.write(
        "lib/lib.go",
        "package lib\n\nfunc Greet() string { return \"hi\" }\n",
    )
    .expect("write lib.go");
    // `gotool: host` lists nothing without a declared variant.
    ws.write(
        "cmd/BUILD",
        "provider_state(\n    provider = \"go\",\n    variants = {\"release\": {\"goos\": \"linux\", \"goarch\": \"amd64\"}},\n)\n",
    )
    .expect("write BUILD");

    let stub = tempfile::tempdir().expect("stub dir");
    let go = stub.path().join("go");
    std::fs::write(&go, "#!/bin/sh\necho 'stub go: must not run' >&2\nexit 1\n")
        .expect("write stub go");
    std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).expect("chmod stub go");
    let path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut dirs = vec![stub.path().to_path_buf()];
            dirs.extend(std::env::split_paths(&p));
            std::env::join_paths(dirs).expect("join PATH")
        }
        None => stub.path().as_os_str().to_owned(),
    };
    let query_flags = |flags: &[&str], expr: &str, kill_switch: &str| {
        let mut args = vec!["query"];
        args.extend_from_slice(flags);
        args.extend(["-e", expr]);
        ws.cmd(&dist, &args)
            .env("PATH", &path)
            .env("HEPH_NO_LISTED_FACTS", kill_switch)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run")
    };
    let query_with = |expr: &str, kill_switch: &str| query_flags(&[], expr, kill_switch);
    let query = |expr: &str| query_with(expr, "");

    // The control: resolving the specs runs `_golist`, so the stub fails it.
    let out = query_with(r#"//lib/... && driver("nonexistent")"#, "1");
    assert!(
        !out.status.success(),
        "with listed facts off, the stub `go` must fail the walk: {}",
        describe(&out)
    );

    let out = query(r#"//lib/... && driver("nonexistent")"#);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "{}",
        describe(&out)
    );

    // A listed Yes is confirmed by default, which would run the stub; the
    // listing alone is `--candidates`.
    let out = query_flags(&["--candidates"], "//lib/... && driver(go.compile)", "");
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines == ["//lib:build_lib@v=release,vp=cmd"],
        "only the lib's compile is a go.compile target: {}",
        describe(&out)
    );
}

/// Regression test for the log-sink-before-`create` ordering bug: if the host
/// installs its log sink *after* calling the plugin's `create`, a
/// `tracing::error!` logged during construction failure has no subscriber to
/// go to and is silently dropped — right before the ABI seam turns the
/// failure into a non-unwinding abort with zero diagnostic output. Nothing
/// about this is observable in a linked test: it needs a real dlopen'd cdylib
/// whose own statically-linked `tracing` has no subscriber until the host
/// installs one.
///
/// The shipped go plugin already fails construction deterministically given a
/// malformed `walk_db` option (it decodes as a `PathBuf`; a YAML sequence
/// can't deserialize into one) — no purpose-built test hook needed.
#[test]
fn plugin_construction_failure_logs_before_the_abort() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      walk_db: [1, 2, 3]\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(
        !out.status.success(),
        "plugin construction should have failed and aborted: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plugin construction failed") && stderr.contains("walk_db"),
        "construction-failure log was dropped — the log sink must be installed \
         before `create` is called: {}",
        describe(&out)
    );
}

/// A plugin's log line is selected by `HEPH_LOG` under the plugin's own target,
/// through the real seam: the host's filter handed to the cdylib
/// (`heph_plugin_set_log_filter`) and the line coming back over the log sink. The
/// host used to re-emit every plugin event as `heph::plugin`, so neither
/// directive below reached it.
#[test]
fn plugin_log_is_filtered_by_its_own_target() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");
    // The same deterministic construction failure as above: an `error!` from
    // the cdylib's own crate, `plugin_go_cdylib`.
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      walk_db: [1, 2, 3]\n",
        manifest.display()
    ))
    .expect("write config");

    let run_with = |var: &str, filter: &str| {
        let out = ws
            .cmd(&dist, &["inspect", "functions"])
            .env_remove("HEPH_LOG")
            .env_remove("RUST_LOG")
            .env(var, filter)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run");
        let logged = String::from_utf8_lossy(&out.stderr).contains("plugin construction failed");
        (logged, describe(&out))
    };

    let (logged, out) = run_with("HEPH_LOG", "off,plugin_go_cdylib=error");
    assert!(
        logged,
        "everything off but the plugin's own crate: its line must show: {out}"
    );
    let (logged, out) = run_with("HEPH_LOG", "error,plugin_go_cdylib=off");
    assert!(
        !logged,
        "the plugin's own crate turned off: its line must not show: {out}"
    );
    // heph reads `HEPH_LOG` only: a `RUST_LOG` meant for another tool must not
    // turn heph's logging off, so the default filter still shows the error.
    let (logged, out) = run_with("RUST_LOG", "off");
    assert!(logged, "RUST_LOG must not configure heph: {out}");
}

/// The checksum in the manifest is the supply-chain guard on a dylib that is
/// about to be mapped into the process with full privileges. It must reject,
/// loudly, before loading. Nothing about this path exists in a linked test —
/// there is no manifest and no artifact to verify.
#[test]
fn manifest_checksum_mismatch_is_rejected() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");

    let manifest = ws.root().join("heph-go-plugin.json");
    let wrong = format!("sha256:{}", "0".repeat(64));
    write_manifest(&manifest, "go", &dylib, Some(&wrong)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(
        !out.status.success(),
        "loaded a cdylib whose checksum did not match: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checksum"),
        "rejection did not say why: {}",
        describe(&out)
    );
}

/// The oci cdylib, exercised through `inspect def` — which makes the host call
/// the plugin's `parse` across the seam.
///
/// `parse` is where `docker_build` asks buildx for its default platform, i.e. where
/// it shells out. A cdylib's statically-linked tokio is a *different instance*
/// from the host's, and the future the host polls carries none of it, so a
/// reactor touch there panics — and a panic across the ABI seam is a
/// non-unwinding abort that kills `heph` outright, with a backtrace instead of
/// an error. That is invisible to every in-process test: they run on the host's
/// runtime, where the reactor is right there.
///
/// So the assertion is not "the target parses" — without docker it cannot, and
/// that is fine. It is that the process came back at all, with a diagnosable
/// error rather than a SIGABRT.
#[test]
fn shipped_oci_cdylib_parses_across_the_abi_without_aborting() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("oci");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-oci-plugin.json");
    let sum = sha256_file(&dylib).expect("hash oci cdylib");
    write_manifest(&manifest, "oci", &dylib, Some(&sum)).expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");

    // `platforms` is left unset on purpose: that is the branch that probes the
    // builder, and therefore the branch that shells out from `parse`.
    ws.write(
        "pkg/BUILD",
        "target(name = \"df\", driver = \"bash\", run = \"echo 'FROM scratch' > $OUT\", \
         out = \"Dockerfile\")\n\
         target(name = \"img\", driver = \"oci.docker_build\", context = [\":df\"])\n",
    )
    .expect("write BUILD");

    let out = ws
        .run(&dist, &["inspect", "def", "//pkg:img"])
        .expect("run");

    // A non-unwinding abort leaves no exit code (killed by SIGABRT) and prints
    // the panic banner. Either is the failure this test exists to catch.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.code().is_some(),
        "heph was killed by a signal — the plugin panicked across the ABI seam: {}",
        describe(&out)
    );
    assert!(
        !combined.contains("panic in a function that cannot unwind")
            && !combined.contains("there is no reactor running"),
        "plugin `parse` touched the reactor on the host's poll: {}",
        describe(&out)
    );

    // With a builder present the def resolves; without one the probe fails and
    // heph says so. Both are a working seam — an abort is not.
    assert!(
        out.status.success() || combined.contains("docker"),
        "expected a def or a docker-shaped failure, got: {}",
        describe(&out)
    );
}

/// The devenv cdylib loads across the ABI and its driver is reachable.
///
/// Every other shipped cdylib has a load test here; without this one the devenv
/// plugin would be the one artifact heph publishes whose `dlopen` nothing
/// proves — and an ABI mismatch in a cdylib is an abort at load, not an error a
/// user can read.
///
/// Deliberately does not run a `devenv.runner` target: that needs `devenv` and
/// a nix evaluation, which is what `crates/e2e/tests/devenv_runner.rs` covers
/// behind an opt-in. What is asserted here is what only the shipped artifact
/// can answer — that the library maps, the create entry's type report matches,
/// and the driver it exports is registered under the name a BUILD file uses.
#[test]
fn shipped_devenv_cdylib_loads_and_registers_its_driver() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("devenv");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-devenv-plugin.json");
    let sum = sha256_file(&dylib).expect("hash devenv cdylib");
    write_manifest(&manifest, "devenv", &dylib, Some(&sum)).expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");

    ws.write(
        "pkg/BUILD",
        "target(name = \"runner\", driver = \"devenv.runner\", mode = \"wrap\")\n",
    )
    .expect("write BUILD");

    // `inspect def` parses the target without executing it, so this reaches the
    // driver across the seam and stops short of running devenv.
    let out = ws
        .run(&dist, &["inspect", "def", "//pkg:runner"])
        .expect("run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.code().is_some(),
        "heph was killed by a signal — the plugin panicked across the ABI seam: {}",
        describe(&out)
    );
    assert!(
        !combined.contains("driver not found"),
        "the devenv plugin loaded but did not register `devenv.runner`: {}",
        describe(&out)
    );
    assert!(
        out.status.success(),
        "parsing a devenv.runner target should succeed: {}",
        describe(&out)
    );
}

/// Load a shipped cdylib under manifest name `name`, with `options` (YAML
/// lines under `options:`, already indented, or empty).
fn load_as(ws: &Workspace, dist: &Dist, plugin: &str, name: &str, options: &str) {
    let dylib = dist.plugin(plugin);
    assert!(dylib.is_file(), "missing {}", dylib.display());
    let manifest = ws.root().join(format!("heph-{plugin}-plugin.json"));
    let sum = sha256_file(&dylib).expect("hash cdylib");
    write_manifest(&manifest, name, &dylib, Some(&sum)).expect("write manifest");
    let options = if options.is_empty() {
        String::new()
    } else {
        format!("    options:\n{options}")
    };
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n{options}",
        manifest.display()
    ))
    .expect("write config");
}

const GO_OPTIONS: &str = "      gotool: \"host\"\n";

/// **A cdylib's namespace is its manifest's `name`.** The binary reports
/// none: the same go cdylib loaded as `go` answers `heph.go.*`, and loaded as
/// `gopher` answers `heph.gopher.*`. A manifest name that is not a valid
/// `heph.<name>` segment fails the load, naming the name, the rule and the
/// manifest.
#[test]
fn cdylib_namespace_is_its_manifest_name() {
    let dist = Dist::locate();
    for name in ["go", "gopher"] {
        let ws = Workspace::new().expect("workspace");
        load_as(&ws, &dist, "go", name, GO_OPTIONS);
        let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
        assert!(out.status.success(), "{}", describe(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let build_addr: Vec<&str> = stdout
            .lines()
            .filter(|l| l.contains("build_addr("))
            .collect();
        assert_eq!(
            build_addr.len(),
            1,
            "one build_addr, under the manifest name {name}: {}",
            describe(&out)
        );
        assert!(
            build_addr[0].starts_with(&format!("{name}.build_addr(")),
            "{name}: {}",
            describe(&out)
        );
    }

    let ws = Workspace::new().expect("workspace");
    load_as(&ws, &dist, "go", "my-go", GO_OPTIONS);
    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(
        !out.status.success(),
        "loaded a plugin whose manifest name is not a namespace: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let manifest = ws.root().join("heph-go-plugin.json");
    for want in [
        "invalid plugin name \"my-go\"",
        "[a-z_][a-z0-9_]*",
        &manifest.display().to_string(),
    ] {
        assert!(
            stderr.contains(want),
            "missing {want:?}: {}",
            describe(&out)
        );
    }
}

/// The cdylib rows of the spec's name map (the builtin rows are pinned in the
/// root crate's `bootstrap` tests): each shipped driver answers to its new
/// `<plugin>.<local>` name, and the name it had before is an unknown driver.
#[test]
fn shipped_names_match_the_name_map() {
    let dist = Dist::locate();
    /// `(plugin, options, [(new name, old name)])`.
    type Row<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
    let rows: [Row; 3] = [
        (
            "go",
            GO_OPTIONS,
            &[
                ("go.golist", "go_golist"),
                ("go.toolchain", "go_toolchain"),
                ("go.compile", "go_compile"),
                ("go.download", "go_download"),
                ("go.testmain", "go_testmain"),
                ("go.lint", "go_lint"),
                ("go.lint_gate", "go_lint_gate"),
                ("go.lint_fix", "go_lint_fix"),
                ("go.format", "go_format"),
                ("go.format_check", "go_format_check"),
            ],
        ),
        (
            "oci",
            "",
            &[
                ("oci.docker_build", "docker_build"),
                ("oci.image", "oci_image"),
                ("oci.layer", "oci_layer"),
                ("oci.index", "oci_index"),
                ("oci.pull", "oci_pull"),
                ("oci.push", "oci_push"),
                ("oci.load", "oci_load"),
                ("oci.builder_platform", "oci_builder_platform"),
                ("oci.runner", "oci_runner"),
                // Never had a flat name; listed so the shipped cdylib is held
                // to registering them.
                ("oci.exists", "oci_exists"),
                ("oci.tag", "oci_tag"),
            ],
        ),
        ("devenv", "", &[("devenv.runner", "devenv_runner")]),
    ];
    for (plugin, options, names) in rows {
        let ws = Workspace::new().expect("workspace");
        load_as(&ws, &dist, plugin, plugin, options);
        // One target per name; `inspect def` reaches each driver's `parse`
        // without running anything. A config error is fine — it came from the
        // driver — but "driver not found" means the name is not registered.
        let mut build = String::new();
        for (i, (new, old)) in names.iter().enumerate() {
            build.push_str(&format!("target(name = \"new{i}\", driver = \"{new}\")\n"));
            build.push_str(&format!("target(name = \"old{i}\", driver = \"{old}\")\n"));
        }
        ws.write("pkg/BUILD", &build).expect("write BUILD");
        for (i, (new, old)) in names.iter().enumerate() {
            let out = ws
                .run(&dist, &["inspect", "def", &format!("//pkg:new{i}")])
                .expect("run");
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !combined.contains(&format!("driver not found: {new}")),
                "{new} is not registered: {}",
                describe(&out)
            );
            let out = ws
                .run(&dist, &["inspect", "def", &format!("//pkg:old{i}")])
                .expect("run");
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                combined.contains(&format!("driver not found: {old}")),
                "the old name {old} must not resolve: {}",
                describe(&out)
            );
        }
    }
}

/// The C body of a skewed plugin's create and log-sink entries: each exports
/// its stabby probe, and the probe answers "not my layout" — what a plugin
/// built against another `PluginComponents` layout looks like to the host.
const SKEWED_ENTRIES: &str = "void *heph_plugin_create_stabbied_v3(const void *r) { (void)r; return 0; }\n\
     void heph_plugin_create(void) {}\n\
     void *heph_plugin_set_log_sink_stabbied_v3(const void *r) { (void)r; return 0; }\n\
     void heph_plugin_set_log_sink(void) {}\n";

/// The C source of a fixture reporting ABI `version` (none when `None`).
fn fixture_source(version: Option<&str>) -> String {
    match version {
        Some(v) => {
            format!(
                "{SKEWED_ENTRIES}const char *heph_plugin_abi_semver(void) {{ return \"{v}\"; }}\n"
            )
        }
        None => SKEWED_ENTRIES.to_string(),
    }
}

/// Build `source` with the system C compiler as plugin `drift`, load it, and
/// return the dylib's path and the run's stderr. The load must fail (exit 1).
///
/// The system C compiler, because a checked-in binary per platform, or a whole
/// fixture crate staged with the release artifacts, would cost more than the
/// seam it covers.
fn load_refused_fixture(dist: &Dist, source: &str) -> (Workspace, std::path::PathBuf, String) {
    let ws = Workspace::new().expect("workspace");
    let src = ws.root().join("drift.c");
    std::fs::write(&src, source).expect("write fixture source");
    let dylib = ws
        .root()
        .join(format!("heph-drift-plugin.{}", common::DYLIB_EXT));
    let cc = std::process::Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
        .args(["-shared", "-fPIC", "-o"])
        .arg(&dylib)
        .arg(&src)
        .output()
        .expect("run the C compiler (the fixture needs one: set CC)");
    assert!(cc.status.success(), "build fixture: {}", describe(&cc));

    let manifest = ws.root().join("heph-drift-plugin.json");
    write_manifest(&manifest, "drift", &dylib, None).expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");

    let mut cmd = ws.cmd(dist, &["inspect", "functions"]);
    cmd.env("HEPH_LOG", "warn");
    let out = cmd.output().expect("run");
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (ws, dylib, stderr)
}

/// The host's ABI version, as its refusal names it (`; host X.Y.Z`). This crate
/// links no workspace crate, so the version is learned from the binary.
fn host_abi(stderr: &str) -> String {
    assert!(stderr.contains("; host "), "no host version in: {stderr}");
    let (_, rest) = stderr.split_once("; host ").expect("checked above");
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    assert!(
        version.split('.').count() == 3,
        "host version {version:?} in: {stderr}"
    );
    version
}

fn assert_contains_all(stderr: &str, wants: &[&str]) {
    for want in wants {
        assert!(stderr.contains(want), "missing {want:?}: {stderr}");
    }
}

/// **A plugin that reports no ABI version is refused at load, before anything
/// in it is called.** Every plugin since 0.17.0 exports
/// `heph_plugin_abi_semver`; one without it predates the check, and the skew it
/// carries may be one stabby cannot see (0.17.0's was wire-only). Its log-sink
/// entry is never probed: no warning about it means the version check ran
/// first.
#[test]
fn a_plugin_without_an_abi_version_is_refused() {
    let dist = Dist::locate();
    let (_ws, dylib, stderr) = load_refused_fixture(&dist, &fixture_source(None));
    assert_contains_all(
        &stderr,
        &[
            "was built against a different heph ABI (it reports no ABI version, so it \
             predates 0.17.0; host ",
            "rebuild it against this heph",
            &dylib.display().to_string(),
        ],
    );
    assert!(
        !stderr.contains("heph_plugin_set_log_sink"),
        "the refused plugin's entries must not be probed: {stderr}"
    );
}

/// **A plugin reporting an ABI outside the host's range is refused at load**,
/// naming both versions — older and newer alike.
#[test]
fn a_plugin_reporting_another_abi_is_refused() {
    let dist = Dist::locate();
    let (_ws, dylib, stderr) = load_refused_fixture(&dist, &fixture_source(Some("0.1.0")));
    assert_contains_all(
        &stderr,
        &[
            "was built against a different heph ABI (plugin 0.1.0; host ",
            "rebuild it against this heph",
            &dylib.display().to_string(),
        ],
    );
    let (_ws, _, stderr) = load_refused_fixture(&dist, &fixture_source(Some("999.0.0")));
    assert_contains_all(
        &stderr,
        &["(plugin 999.0.0, newer than this heph, which is unsupported; host "],
    );
    let (_ws, _, stderr) = load_refused_fixture(&dist, &fixture_source(Some("0.17")));
    assert_contains_all(
        &stderr,
        &["reports ABI version \"0.17\", which is not a version"],
    );
}

/// **A plugin whose version is accepted but whose layout is not is refused,
/// saying so.** The version check passes (the fixture reports the host's own
/// ABI, then a newer patch of it); stabby's structural check on
/// `heph_plugin_create` then refuses it.
/// The log-sink entry with a mismatched report is warned about rather than
/// skipped in silence.
#[test]
fn a_plugin_from_another_abi_is_refused() {
    let dist = Dist::locate();
    let (_ws, _, probe) = load_refused_fixture(&dist, &fixture_source(Some("0.1.0")));
    let host = host_abi(&probe);

    let (_ws, dylib, stderr) = load_refused_fixture(&dist, &fixture_source(Some(&host)));
    assert_contains_all(
        &stderr,
        &[
            &format!("was built against a different heph ABI (host {host})"),
            "reinstall plugins from the same release",
            &dylib.display().to_string(),
            // The optional entry with a mismatched type is warned about.
            "heph_plugin_set_log_sink",
        ],
    );

    // A newer *patch* is additive: the version check lets it through (so it
    // reaches the same layout refusal), never "newer than this heph".
    let mut parts = host.split('.');
    let (major, minor) = (parts.next().expect("major"), parts.next().expect("minor"));
    let patch = format!("{major}.{minor}.99");
    let (_ws, _, stderr) = load_refused_fixture(&dist, &fixture_source(Some(&patch)));
    assert_contains_all(
        &stderr,
        &[&format!(
            "was built against a different heph ABI (host {host})"
        )],
    );
    assert!(
        !stderr.contains("newer than this heph"),
        "a newer patch is in the lane: {stderr}"
    );
}

/// **The release binary hands a real cdylib driver its allotment.**
///
/// The allotment rides `ManagedRunRequest.allotment` (field 15, since 0.17.4)
/// from the host's run request, through the stabby seam, into the cdylib's own
/// managed layer. `convert.rs`'s round-trip proves the encoding; only a real
/// `dlopen` proves the host fills the field and the plugin's copy of the SDK
/// reads it. The test cdylib's `allot` driver writes what it was told into its
/// output (each granted pool as `<pool>=<amount>`, or `told-nothing`).
///
/// Two rows under `-j 2`: `cpu: 2` with `memory: "1G"` is told both; `cpu: 16`
/// is clamped to the capacity and told 2, and no memory since it reserved
/// none.
#[test]
fn allotment_crosses_the_plugin_seam() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.test_plugin("entrypoint");
    assert!(dylib.is_file(), "missing {}", dylib.display());
    let manifest = ws.root().join("test-entrypoint-plugin.json");
    let sum = sha256_file(&dylib).expect("hash test cdylib");
    write_manifest(&manifest, "entrypoint_test", &dylib, Some(&sum)).expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");
    ws.write(
        "app/BUILD",
        concat!(
            "target(name = \"wide\", driver = \"entrypoint_test.allot\", ",
            "resources = {\"cpu\": 2, \"memory\": \"1G\"})\n",
            "target(name = \"clamped\", driver = \"entrypoint_test.allot\", ",
            "resources = {\"cpu\": 16})\n",
        ),
    )
    .expect("write BUILD");

    for (addr, want) in [
        ("//app:wide", "cpu=2 memory=1073741824"),
        ("//app:clamped", "cpu=2"),
    ] {
        let out = ws
            .run(&dist, &["-j", "2", "run", addr, "--cat-out"])
            .expect("run");
        assert!(out.status.success(), "{addr}: {}", describe(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            want,
            "{addr}: the cdylib driver was not handed the run's allotment: {}",
            describe(&out)
        );
    }
}

/// **A cdylib driver's host paths reach the manifest across a real `dlopen`;
/// a plugin reporting ABI 0.21.1 declares none.**
///
/// The test cdylib's `hostpaths` driver declares the path its target names
/// and writes `run=<nanoseconds>`, so equal bytes are a hit and new bytes a
/// re-run. At this ABI: a second run hits, and with the path deleted the entry
/// is stale and re-runs. With `HEPH_TEST_PLUGIN_ABI_0_21_1` the cdylib reports
/// 0.21.1 and behaves as a plugin from before host paths: the entry outlives
/// the path.
#[test]
fn cdylib_host_paths_cross_abi() {
    let dist = Dist::locate();
    let dylib = dist.test_plugin("entrypoint");
    assert!(dylib.is_file(), "missing {}", dylib.display());
    for old in [false, true] {
        let ws = Workspace::new().expect("workspace");
        let manifest = ws.root().join("test-entrypoint-plugin.json");
        let sum = sha256_file(&dylib).expect("hash test cdylib");
        write_manifest(&manifest, "entrypoint_test", &dylib, Some(&sum)).expect("write manifest");
        ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
            .expect("write config");
        let store = ws.root().join("store");
        std::fs::create_dir_all(&store).expect("mkdir store");
        let path = store.join("env");
        std::fs::write(&path, b"").expect("create host path");
        ws.write(
            "app/BUILD",
            &format!(
                "target(name = \"t\", driver = \"entrypoint_test.hostpaths\", path = {:?})\n",
                path.display().to_string()
            ),
        )
        .expect("write BUILD");

        let run = || -> String {
            let mut cmd = ws.cmd(&dist, &["run", "//app:t", "--cat-out"]);
            if old {
                cmd.env("HEPH_TEST_PLUGIN_ABI_0_21_1", "1");
            }
            let child = cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn heph");
            let out = common::wait_output(child, common::RUN_DEADLINE).expect("heph run");
            assert!(out.status.success(), "old={old}: {}", describe(&out));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let first = run();
        assert!(first.starts_with("run="), "old={old}: {first}");
        assert_eq!(run(), first, "old={old}: the second run hits");

        std::fs::remove_file(&path).expect("delete host path");
        let after = run();
        if old {
            assert_eq!(
                after, first,
                "a 0.21.1 plugin declared nothing: still a hit"
            );
        } else {
            assert_ne!(
                after, first,
                "the declared path is gone: the entry is stale"
            );
        }
    }
}
