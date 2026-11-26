#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `HEPH_SESSION_EXPORT_DIR` never reaches a target (session-store spec I1,
//! E4; `docs/SESSIONS.md`, "In CI").
//!
//! Its own binary because the exec driver reads the *process* environment, for
//! `pass_env = ["*"]` at parse and `runtime_pass_env = ["*"]` at run: the only
//! way to put the variable in front of it is `std::env::set_var`, which every
//! other test of a shared binary would see while it is set. `plugin-exec`
//! unit-tests the strip itself as a function over an environment
//! (`wildcard_strips_the_session_export_dir`) and the refusal of a named
//! `pass_env`; this proves the driver's real paths go through them.
//!
//! A session runner's launch is not exercised here: it starts from
//! `env_clear()`, so the variable reaches it only through `launch_env`, which
//! `execrunner`'s `the_launch_never_carries_the_session_export_dir` covers.

mod common;

use common::Workspace;
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use std::sync::Arc;

/// `hcore::session_record::SESSION_EXPORT_DIR_ENV`, spelled out: `e2e` does
/// not depend on `core`, and a test that pins the name is worth the copy.
const EXPORT_DIR: &str = "HEPH_SESSION_EXPORT_DIR";

/// A variable of the test's own, present in both phases: it proves the
/// wildcards pass the host environment through at all, so the export
/// directory's absence from the same output means it was stripped.
const PROBE: &str = "E2E_EXPORT_DIR_PROBE";

/// Each target writes what it saw of both variables to its one output.
const BUILD: &str = r#"
target(
    name = "runtime",
    driver = "bash",
    run = "echo probe=${E2E_EXPORT_DIR_PROBE:-absent} export=${HEPH_SESSION_EXPORT_DIR:-absent} > $OUT",
    out = "env.txt",
    runtime_pass_env = ["*"],
)
target(
    name = "hashed",
    driver = "bash",
    run = "echo probe=${E2E_EXPORT_DIR_PROBE:-absent} export=${HEPH_SESSION_EXPORT_DIR:-absent} > $OUT",
    out = "env.txt",
    pass_env = ["*"],
)
target(
    name = "named",
    driver = "bash",
    run = "true",
    pass_env = ["HEPH_SESSION_EXPORT_DIR"],
)
target(
    name = "runtime_named",
    driver = "bash",
    run = "echo probe=${E2E_EXPORT_DIR_PROBE:-absent} export=${HEPH_SESSION_EXPORT_DIR:-absent} > $OUT",
    out = "env.txt",
    runtime_pass_env = ["HEPH_SESSION_EXPORT_DIR"],
)
"#;

/// Sets a host variable until dropped.
struct EnvVar(&'static str);

impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        // SAFETY: this binary holds one test, so no other thread reads the
        // environment while it changes; the guard restores it on drop.
        unsafe { std::env::set_var(name, value) };
        Self(name)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

/// What a phase saw of one target: its output and every hash a cache key or a
/// consumer could carry.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    output: String,
    def_hash: Vec<u8>,
    hashin: String,
    hashout: Vec<String>,
}

/// Build `addr` on `engine` (forced, so a phase never reads the other's
/// cache entry) and read its hashes.
async fn seen(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<Seen> {
    let addr = parse_addr(addr)?;
    let forced = ResultOptions {
        force: true,
        ..ResultOptions::default()
    };
    let result = Arc::clone(engine)
        .result_addr(engine.new_state(), &addr, OutputMatcher::All, &forced)
        .await?;
    let def = Arc::clone(engine)
        .get_def(engine.new_state(), &addr)
        .await?;
    let meta = Arc::clone(engine).meta(engine.new_state(), &addr).await?;
    Ok(Seen {
        output: common::artifact_string(&result).trim().to_string(),
        def_hash: def.target_def.hash.clone(),
        hashin: meta.hashin,
        hashout: result
            .artifacts
            .iter()
            .map(|a| a.hashout())
            .collect::<anyhow::Result<_>>()?,
    })
}

/// A target under `runtime_pass_env = ["*"]`, and one under the hashed
/// `pass_env = ["*"]`, write what they see: neither sees the export directory,
/// and the def hash, `hashin` and hashout are the same with and without it.
/// Naming it in `pass_env` fails at parse; naming it in `runtime_pass_env`
/// passes it, with no hash moving between two values.
#[tokio::test]
async fn export_dir_never_reaches_a_target() -> anyhow::Result<()> {
    let _probe = EnvVar::set(PROBE, "1");
    let ws = Workspace::new();
    ws.write_build_file("p", BUILD);
    let export_dir = tempfile::tempdir()?;
    let export_path = export_dir
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("a non-UTF-8 tempdir"))?;
    let addrs = ["//p:runtime", "//p:hashed"];

    let with_var = EnvVar::set(EXPORT_DIR, export_path);
    let mut with = Vec::new();
    for addr in addrs {
        let s = seen(&ws.engine, addr).await?;
        assert_eq!(
            s.output, "probe=1 export=absent",
            "{addr} under a wildcard must see the host's variables but not {EXPORT_DIR}"
        );
        with.push(s);
    }
    let named = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &parse_addr("//p:named")?)
        .await;
    let Err(e) = named else {
        panic!("naming {EXPORT_DIR} in a hashed pass_env must fail at parse");
    };
    let e = format!("{e:#}");
    assert!(
        e.contains(EXPORT_DIR) && e.contains("runtime_pass_env"),
        "the refusal names the variable and the way out: {e}"
    );
    drop(with_var);
    // CI sets it on the step that runs the tests; the guard removed it.
    assert!(std::env::var_os(EXPORT_DIR).is_none());

    // A fresh engine: the memoized def of the first one would hide a
    // difference in the parse-time `pass_env = ["*"]` snapshot.
    let engine = ws.reopen()?;
    for (addr, with) in addrs.into_iter().zip(&with) {
        let without = seen(&engine, addr).await?;
        assert_eq!(
            &without, with,
            "{addr}: {EXPORT_DIR} must change neither the output nor a hash"
        );
    }
    drop(engine);

    // The exemption (E4): a target that names it in `runtime_pass_env`,
    // which is not hashed, sees the value, and the directory a job picked
    // moves neither its def hash nor its `hashin`. A fresh engine per value,
    // as above. The probe is not named, so it does not reach this one.
    let mut keys = Vec::new();
    for value in ["/a", "/b"] {
        let _var = EnvVar::set(EXPORT_DIR, value);
        let engine = ws.reopen()?;
        let s = seen(&engine, "//p:runtime_named").await?;
        assert_eq!(s.output, format!("probe=absent export={value}"));
        keys.push((s.def_hash, s.hashin));
    }
    assert_eq!(
        keys.first(),
        keys.get(1),
        "{EXPORT_DIR} in runtime_pass_env must move neither the def hash nor hashin"
    );
    Ok(())
}
