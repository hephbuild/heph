//! `devenv_runner` — a target that describes a [devenv](https://devenv.sh)
//! environment, so other targets can run inside it.
//!
//! The output is a `runner.json`; that is the entire contract. Targets name
//! this target with `runner = "//tools/devenv:runner"`, or a whole workspace
//! does it once with the `exec`/`bash` driver's `runner:` option.
//!
//! # The plugin has no runner code, and that is deliberate
//!
//! Both modes reuse a builtin runner:
//!
//! - `wrap` captures the environment once, at *runner build time*, and emits it
//!   as a literal env map. Targets then spawn locally with that environment —
//!   no `devenv` process per target, no shell evaluation on the hot path.
//! - `session` emits a `launch` argv and lets the builtin `session` runner hold
//!   one `devenv shell` open for the whole build, running targets inside it via
//!   the agent protocol.
//!
//! What this driver contributes is the *fingerprint*, and getting that right is
//! the whole job (see [`fingerprint`](self#fingerprinting)).
//!
//! # Which mode
//!
//! `wrap` is the one most workspaces want: it is faster, it needs no agent, and
//! its fingerprint is the strongest available because it *is* the environment.
//! `session` earns its cost when the environment is not a set of variables —
//! shell activation with side effects, services devenv starts, state under
//! `.devenv/` — i.e. when what matters is process ancestry rather than
//! `environ`.
//!
//! # Fingerprinting
//!
//! A consumer's cache key comes from this target's *hashout* — the bytes of the
//! `runner.json`. If those bytes did not move when the environment did, every
//! consumer would keep serving artifacts built in the old one. So both modes
//! resolve the environment and fold a digest of it into the file, rather than
//! hashing `devenv.nix` and hoping: `devenv.nix` is Nix source and can `import
//! ./nix/rust.nix` or read `devenv.local.nix`, none of which the declared inputs
//! see.
//!
//! The capture must also be a *pure function of declared inputs*, or the runner
//! target's own hashout drifts per machine and every consumer full-misses
//! forever. So `devenv` runs under a cleared environment populated only from
//! [`TargetSpec::pass_env`], whose values are snapshotted and hashed at parse.
//! Given a pinned input environment the output is deterministic, which is what
//! makes taking the captured environment wholesale (minus per-process noise)
//! sound rather than a denylist gamble.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hexecrunner::RunnerRef;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use hproc::proc_exec;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "devenv_runner";

/// Bump to re-derive every runner this driver produced, when the shape of what
/// it writes changes.
///
/// 2: a `session` target's environment now reaches it. The shape of this file
/// did not change — the agent did (it composes a target's environment from its
/// own, which is the shell the launch put it in, instead of starting from an
/// empty one) — but every result produced under a session runner before that
/// was built without the environment, so they have to re-derive rather than be
/// served from the cache.
const FORMAT_VERSION: u32 = 2;

/// The file a runner target must produce.
const OUT_FILE: &str = "runner.json";

/// Host variables `devenv` itself needs to resolve an environment: to find the
/// nix store and channels, to validate TLS for flake fetches, and to route
/// through a corporate proxy. Mirrors the list `plugin-nix` passes to `nix`.
///
/// These are the *default* `pass_env`. They are snapshotted and hashed at parse
/// like any other `pass_env`, so the capture stays a pure function of declared
/// inputs — and so switching nixpkgs channels re-derives rather than serving a
/// runner built against the old one.
const DEFAULT_PASS_ENV: &[&str] = &[
    "HOME",
    "USER",
    "PATH",
    "NIX_PATH",
    "NIX_SSL_CERT_FILE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
];

/// Variables dropped from the *captured* environment.
///
/// Everything here varies per invocation or per directory, and the runner's
/// output is a cache key: one of these surviving makes the hashout move on
/// every build and full-misses every consumer in the workspace, forever, with
/// nothing erroring and nothing pointing at the cause.
///
/// The `DEVENV_*` entries are not just noise, they are actively wrong to pass
/// on. They point at the runner target's **sandbox**, which is deleted as soon
/// as the target is cached — so a consumer receiving them would get paths that
/// no longer exist. `DEVENV_PROFILE` is deliberately absent from this list: it
/// is a content-addressed store path, it is stable, and it is what the
/// fingerprint is derived from.
///
/// This is a denylist, which is only sound because the *input* environment is a
/// strict hashed allowlist: with the input pinned, what devenv adds is a
/// function of the declared inputs plus these known-volatile few.
const CAPTURE_DENY: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TMPDIR",
    "TMP",
    "TEMP",
    // Per-invocation or per-directory devenv bookkeeping.
    "DEVENV_CMDLINE",
    "DEVENV_DOTFILE",
    "DEVENV_ROOT",
    "DEVENV_RUNTIME",
    "DEVENV_STATE",
    "DEVENV_TASK_FILE",
];

/// The variable carrying devenv's resolved profile — a content-addressed nix
/// store path, and the strongest fingerprint available.
const DEVENV_PROFILE: &str = "DEVENV_PROFILE";

/// Strip per-invocation noise from *inside* a value.
///
/// `NIX_CFLAGS_COMPILE` carries `-frandom-seed=<token>`, which nix regenerates
/// on every invocation. A name-based denylist cannot see it — the variable is
/// wanted, only that fragment is volatile — and leaving it in was enough on its
/// own to make the capture differ between two evaluations of an identical
/// environment. The seed only feeds symbol-name generation, so dropping it
/// costs nothing.
fn normalize_value(value: &str) -> String {
    if !value.contains("-frandom-seed=") {
        return value.to_string();
    }
    value
        .split_whitespace()
        .filter(|tok| !tok.starts_with("-frandom-seed="))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Spec)]
pub(crate) struct TargetSpec {
    /// `wrap` (default) captures the environment and runs targets locally in it.
    /// `session` holds one `devenv shell` open and runs targets inside it.
    pub mode: String,
    /// Directory containing `devenv.nix`, relative to this package. Defaults to
    /// the package directory.
    pub root: String,
    /// devenv profile to enter, if any.
    pub profile: String,
    /// The environment's own files (`devenv.nix`, `devenv.lock`, `devenv.yaml`,
    /// anything they import), as target addresses — typically a `glob`.
    ///
    /// These make the runner rebuild when the environment's definition changes.
    /// They are not what the fingerprint is derived from: a `devenv.nix` can
    /// import files nobody declared, so the fingerprint comes from the
    /// *resolved* environment instead.
    pub deps: Vec<String>,
    /// Host environment variables `devenv` may see, hashed at parse.
    /// Defaults to the set devenv needs to reach the nix store and the network.
    pub pass_env: Vec<String>,
}

/// What `run` needs, and what the def hash is taken over.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct DevenvDef {
    pub mode: Mode,
    pub root: String,
    pub profile: String,
    /// Snapshotted at parse and hashed — this is what keeps the capture a pure
    /// function of declared inputs.
    pub pass_env: BTreeMap<String, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    Wrap,
    Session,
}

impl Mode {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "" | "wrap" => Ok(Self::Wrap),
            "session" => Ok(Self::Session),
            other => anyhow::bail!(
                "devenv_runner: unknown mode {other:?}; expected \"wrap\" (capture the \
                 environment, run targets locally in it) or \"session\" (hold one `devenv shell` \
                 open and run targets inside it)"
            ),
        }
    }
}

#[derive(Debug)]
pub struct Driver {
    /// The `devenv` binary. From the driver's `bin:` option so a workspace can
    /// pin it; `devenv` off `PATH` otherwise.
    devenv_bin: String,
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Self {
            devenv_bin: "devenv".to_string(),
        }
    }

    pub fn from_options(opts: &hplugin::config::Options) -> anyhow::Result<Self> {
        hplugin::config::deny_unknown("devenv_runner driver", opts, &["bin"])?;
        let bin: Option<String> = hplugin::config::decode_opt(opts, "devenv_runner driver", "bin")?;
        Ok(Self {
            devenv_bin: bin
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "devenv".to_string()),
        })
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        TargetSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = TargetSpec::from(&req.target_spec.config)?;
        let pkg = req.target_spec.addr.package.clone();
        let mode = Mode::parse(&spec.mode)?;
        check_root(&spec.root)?;

        let names: Vec<String> = if spec.pass_env.is_empty() {
            DEFAULT_PASS_ENV.iter().map(|s| (*s).to_string()).collect()
        } else {
            spec.pass_env.clone()
        };
        // Snapshotted here, not read in `run`: these are *inputs*, and hashing
        // them is what makes a changed `NIX_PATH` re-derive the environment
        // instead of serving one captured against the old channel.
        let pass_env: BTreeMap<String, String> = names
            .into_iter()
            .filter_map(|n| std::env::var(&n).ok().map(|v| (n, v)))
            .collect();

        let def = DevenvDef {
            mode,
            root: spec.root.clone(),
            profile: spec.profile.clone(),
            pass_env,
        };

        let mut h = Xxh3::new();
        h.update(b"devenv_runner");
        h.update(&FORMAT_VERSION.to_le_bytes());
        h.update(format!("{:?}", def.mode).as_bytes());
        h.update(def.root.as_bytes());
        h.update(def.profile.as_bytes());
        h.update(self.devenv_bin.as_bytes());
        for (k, v) in &def.pass_env {
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(v.as_bytes());
            h.update(b"\x1f");
        }
        let hash = format!("{:x}", h.digest()).into_bytes();

        let inputs = spec
            .deps
            .iter()
            .enumerate()
            .map(|(i, d)| {
                Ok(Input {
                    r#ref: TargetAddr::parse(d, &pkg)?,
                    mode: InputMode::Standard,
                    origin_id: format!("devenv|{i}"),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let out_path = hmodel::htpkg::join_rel_checked(pkg.as_str(), OUT_FILE)
            .with_context(|| format!("resolving {OUT_FILE} in package {pkg}"))?;

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![OutPath {
                        content: Content::FilePath(out_path),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                support_files: vec![],
                // Local yes, remote never. The captured environment names this
                // machine's nix store paths; publishing it would let one host's
                // answer key another's builds, which is the cross-machine
                // mix-up the fingerprint exists to prevent.
                cache: CacheConfig::on(false),
                pty: false,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<DevenvDef>().clone();

        // The devenv root, inside the sandbox: the declared deps were
        // materialized here, so this is the copy whose content the build
        // depends on.
        let root = if def.root.is_empty() {
            req.sandbox_pkg_dir.clone()
        } else {
            req.sandbox_pkg_dir.join(&def.root)
        };

        let captured = self
            .capture_env(&root, &def, ctoken)
            .await
            .with_context(|| {
                format!(
                    "resolving the devenv environment in {root:?}. Both runner modes need it: \
                     `wrap` uses it directly, and `session` needs its digest as the fingerprint \
                     every consumer's cache key rests on."
                )
            })?;

        let fingerprint = fingerprint_of(&captured);
        let config = self.runner_config(&def, self.real_root(&req, &def), captured);

        let doc = serde_json::json!({
            "version": 1,
            "fingerprint": fingerprint,
            "runner": match def.mode { Mode::Wrap => "wrap", Mode::Session => "session" },
            "config": config,
        });

        let out = req.sandbox_pkg_dir.join(OUT_FILE);
        tokio::fs::write(&out, serde_json::to_vec_pretty(&doc)?)
            .await
            .with_context(|| format!("write {out:?}"))?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

impl Driver {
    /// The runner-specific half of `runner.json`.
    ///
    /// Extracted from `run` so both shapes are assertable without a sandbox, a
    /// nix store, or a `devenv` on PATH — the capture is the slow, impure part,
    /// and what it is turned *into* is the part that decides whether a target
    /// gets an environment.
    /// `wrap` hands the environment over as data; `session` hands over the argv
    /// that enters it and lets the agent's own `environ` be the environment (see
    /// `hexecrunner::agent`). Both fingerprint the same capture.
    fn runner_config(
        &self,
        def: &DevenvDef,
        real_root: String,
        captured: BTreeMap<String, String>,
    ) -> serde_json::Value {
        match def.mode {
            Mode::Wrap => serde_json::json!({
                "env": captured,
            }),
            Mode::Session => {
                let mut launch = vec![self.devenv_bin.clone()];
                if !def.profile.is_empty() {
                    launch.push("--profile".to_string());
                    launch.push(def.profile.clone());
                }
                launch.push("shell".to_string());
                launch.push("--".to_string());
                serde_json::json!({
                    "launch": launch,
                    // The launch resolves its environment relative to where it
                    // runs, and the runner target's sandbox is gone by then —
                    // so point it at the real tree, not the sandbox copy.
                    "cwd": real_root,
                })
            }
        }
    }

    /// The devenv root in the *real* tree.
    ///
    /// A session outlives the runner target's sandbox, which is deleted as soon
    /// as the target is cached — so the launch command has to point at the
    /// workspace, not at a directory that will be gone.
    fn real_root(&self, req: &ManagedRunRequest<'_, '_>, def: &DevenvDef) -> String {
        let pkg = req.request.target.addr.package.as_str();
        let mut p = req.request.tree_root_path.clone();
        if !pkg.is_empty() {
            p = p.join(pkg);
        }
        if !def.root.is_empty() {
            p = p.join(&def.root);
        }
        p.to_string_lossy().into_owned()
    }

    /// Ask devenv what environment it provides.
    ///
    /// `env -0`, not `env`: a value may legitimately contain a newline, and a
    /// line-split capture would tear it in half and hand targets a truncated
    /// variable.
    async fn capture_env(
        &self,
        root: &std::path::Path,
        def: &DevenvDef,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut args: Vec<OsString> = Vec::new();
        if !def.profile.is_empty() {
            args.push(OsString::from("--profile"));
            args.push(OsString::from(&def.profile));
        }
        args.push(OsString::from("shell"));
        args.push(OsString::from("--"));
        args.push(OsString::from("env"));
        args.push(OsString::from("-0"));

        let spec = proc_exec::Spec {
            program: PathBuf::from(&self.devenv_bin),
            args,
            // Cleared, populated only from the hashed snapshot. This is what
            // makes the capture reproducible rather than a photograph of
            // whoever's shell happened to run the build.
            env: def
                .pass_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            cwd: root.to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: true,
            ctty: false,
        };

        let out = hexecrunner::output(RunnerRef::local(), spec, ctoken)
            .await
            .with_context(|| format!("run `{} shell -- env -0`", self.devenv_bin))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("`{} shell` failed in {root:?}: {stderr}", self.devenv_bin);
        }
        Ok(parse_env0(&out.stdout))
    }
}

/// Reject a `root` that would escape the sandbox.
///
/// `run` resolves the devenv root as `sandbox_pkg_dir.join(root)`, and
/// `PathBuf::join` *discards the base* when handed an absolute path — so a bare
/// `root = "/etc"` would evaluate a devenv outside the sandbox entirely, and
/// `..` would climb out of it. Both would make the capture depend on tree state
/// the target never declared, which is the whole thing the declared deps exist
/// to prevent. Caught at parse so the error names the field rather than
/// surfacing as a confusing evaluation somewhere else on the disk.
fn check_root(root: &str) -> anyhow::Result<()> {
    let path = std::path::Path::new(root);
    if path.is_absolute() {
        anyhow::bail!(
            "devenv_runner: `root` must be relative to the package, got the absolute path \
             {root:?}. An absolute root would evaluate a devenv outside the sandbox, against \
             files this target never declared."
        );
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "devenv_runner: `root` must stay inside the package, got {root:?}. A `..` component \
             climbs out of the sandbox, against files this target never declared."
        );
    }
    Ok(())
}

/// Parse NUL-separated `KEY=VALUE` records, dropping per-process noise.
fn parse_env0(bytes: &[u8]) -> BTreeMap<String, String> {
    bytes
        .split(|b| *b == 0)
        .filter(|rec| !rec.is_empty())
        .filter_map(|rec| {
            let s = std::str::from_utf8(rec).ok()?;
            let (k, v) = s.split_once('=')?;
            if CAPTURE_DENY.contains(&k) {
                return None;
            }
            Some((k.to_string(), normalize_value(v)))
        })
        .collect()
}

/// A fingerprint for the resolved environment.
///
/// Derived, never authored, and derived from what devenv actually produced
/// rather than from the files it was asked to read — `devenv.nix` can import
/// files nobody declared, so a source-file hash would miss exactly the change
/// that matters.
///
/// Prefers `DEVENV_PROFILE`, the nix store path devenv resolved the environment
/// to. It is content-addressed, so it changes when and only when the
/// environment changes, and it is identical across machines and directories —
/// which a digest of the whole environment is not, however carefully the
/// volatile parts are filtered. The digest is the fallback for a devenv that
/// does not export it.
fn fingerprint_of(env: &BTreeMap<String, String>) -> String {
    if let Some(profile) = env.get(DEVENV_PROFILE)
        && let Some(hash) = profile.rsplit('/').next()
        && !hash.is_empty()
    {
        return format!("devenv:{hash}");
    }
    let mut h = Xxh3::new();
    h.update(b"devenv/v1");
    for (k, v) in env {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update(b"\x1f");
    }
    format!("devenv:{:016x}", h.digest())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_with_wrap_as_the_default() {
        assert_eq!(Mode::parse("").expect("default"), Mode::Wrap);
        assert_eq!(Mode::parse("wrap").expect("wrap"), Mode::Wrap);
        assert_eq!(Mode::parse("session").expect("session"), Mode::Session);
    }

    fn def(mode: Mode) -> DevenvDef {
        DevenvDef {
            mode,
            root: String::new(),
            profile: String::new(),
            pass_env: BTreeMap::new(),
        }
    }

    fn captured() -> BTreeMap<String, String> {
        BTreeMap::from([("APP_CHANNEL".to_string(), "stable".to_string())])
    }

    /// Each mode carries the environment the way that mode can use it.
    ///
    /// `wrap` spawns targets here, so the environment has to be data. `session`
    /// puts an agent inside the environment, so the agent's own `environ` is the
    /// environment — declaring a copy would be a second thing to keep in sync
    /// with the first, and the fingerprint (which both modes derive from the
    /// same capture) is what the consumer's cache key rests on either way.
    #[test]
    fn wrap_carries_the_environment_and_session_carries_the_way_in() {
        let d = Driver::new();

        let wrap = d.runner_config(&def(Mode::Wrap), "/ws".to_string(), captured());
        assert_eq!(wrap["env"]["APP_CHANNEL"], "stable");

        let session = d.runner_config(&def(Mode::Session), "/ws".to_string(), captured());
        assert_eq!(session["cwd"], "/ws");
        assert_eq!(
            session["launch"],
            serde_json::json!(["devenv", "shell", "--"])
        );
        assert!(
            session.get("env").is_none(),
            "the agent's own environ is the environment; a declared copy would drift from it"
        );
    }

    /// The launch has to resolve the environment somewhere that still exists
    /// when a target runs — the runner target's sandbox is deleted as soon as it
    /// is cached.
    #[test]
    fn a_session_launch_points_at_the_real_tree() {
        let cfg = Driver::new().runner_config(
            &def(Mode::Session),
            "/real/tree".to_string(),
            BTreeMap::new(),
        );
        assert_eq!(cfg["cwd"], "/real/tree");
    }

    #[test]
    fn an_unknown_mode_explains_both_options() {
        let err = Mode::parse("agent").expect_err("must reject");
        let msg = format!("{err:#}");
        assert!(msg.contains("wrap"), "{msg}");
        assert!(msg.contains("session"), "{msg}");
    }

    /// `env -0`, because a value may contain a newline and a line-split capture
    /// would hand the target half of it.
    #[test]
    fn env0_survives_a_newline_in_a_value() {
        let raw = b"A=1\nstill-a\0B=2\0";
        let env = parse_env0(raw);
        assert_eq!(env.get("A").map(String::as_str), Some("1\nstill-a"));
        assert_eq!(env.get("B").map(String::as_str), Some("2"));
    }

    /// Per-process noise must not reach the capture: it would move the runner's
    /// hashout on every invocation, and every consumer would full-miss forever
    /// with nothing pointing at why.
    #[test]
    fn per_process_noise_is_dropped() {
        let raw = b"PWD=/a\0SHLVL=3\0_=/usr/bin/env\0OLDPWD=/b\0KEEP=yes\0";
        let env = parse_env0(raw);
        assert_eq!(env.keys().collect::<Vec<_>>(), vec!["KEEP"]);
    }

    /// The three ways a real capture was non-deterministic, before an e2e test
    /// against a real devenv caught them. Any one of them makes every consumer
    /// in the workspace full-miss forever, silently.
    #[test]
    fn the_volatile_parts_of_a_real_capture_are_dropped() {
        let raw = b"DEVENV_ROOT=/tmp/a\0DEVENV_DOTFILE=/tmp/a/.devenv\0DEVENV_STATE=/tmp/a/.devenv/state\0DEVENV_RUNTIME=/tmp/devenv-c555f66\0DEVENV_TASK_FILE=/nix/store/zzz-tasks.json\0DEVENV_CMDLINE=shell -- env -0\0DEVENV_PROFILE=/nix/store/qb2i0-devenv-profile\0KEEP=yes\0";
        let env = parse_env0(raw);
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            vec!["DEVENV_PROFILE", "KEEP"],
            "only the content-addressed profile and real variables survive"
        );
    }

    /// `-frandom-seed` lives *inside* `NIX_CFLAGS_COMPILE`, so no name-based
    /// filter can see it — and on its own it was enough to make two evaluations
    /// of an identical environment differ.
    #[test]
    fn the_random_seed_is_stripped_from_inside_a_value() {
        let a = normalize_value("-frandom-seed=35f61pidv5 -isystem /nix/store/x/include");
        let b = normalize_value("-frandom-seed=wqqzvajh47 -isystem /nix/store/x/include");
        assert_eq!(a, b);
        assert_eq!(a, "-isystem /nix/store/x/include");
    }

    #[test]
    fn a_value_without_a_seed_is_untouched() {
        let v = "-isystem  /nix/store/x/include";
        assert_eq!(normalize_value(v), v);
    }

    /// The profile is a content-addressed store path, so it is the same on
    /// every machine and in every directory for a given environment — which a
    /// digest of the whole environment is not, however carefully filtered.
    #[test]
    fn the_fingerprint_prefers_the_profile_store_path() {
        let env = BTreeMap::from([
            (
                DEVENV_PROFILE.to_string(),
                "/nix/store/qb2i0ily2jm27sv7qckfs8sjsylnrp5n-devenv-profile".to_string(),
            ),
            ("PATH".to_string(), "/whatever".to_string()),
        ]);
        assert_eq!(
            fingerprint_of(&env),
            "devenv:qb2i0ily2jm27sv7qckfs8sjsylnrp5n-devenv-profile"
        );

        // And it tracks the profile rather than the rest of the environment.
        let mut other = env.clone();
        other.insert("PATH".to_string(), "/different".to_string());
        assert_eq!(fingerprint_of(&env), fingerprint_of(&other));
    }

    #[test]
    fn an_empty_capture_still_fingerprints() {
        assert!(fingerprint_of(&BTreeMap::new()).starts_with("devenv:"));
    }

    /// The fingerprint must move when the environment does — that is the whole
    /// reason it exists, and the property a consumer's cache key rests on.
    #[test]
    fn the_fingerprint_tracks_the_environment() {
        let a = BTreeMap::from([("PATH".to_string(), "/nix/store/aaa/bin".to_string())]);
        let b = BTreeMap::from([("PATH".to_string(), "/nix/store/bbb/bin".to_string())]);
        assert_ne!(fingerprint_of(&a), fingerprint_of(&b));
        assert_eq!(fingerprint_of(&a), fingerprint_of(&a.clone()));
    }

    /// A variable appearing or vanishing is an environment change too.
    #[test]
    fn the_fingerprint_notices_an_added_variable() {
        let a = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let mut b = a.clone();
        b.insert("B".to_string(), "2".to_string());
        assert_ne!(fingerprint_of(&a), fingerprint_of(&b));
    }

    /// `PathBuf::join` discards the base when given an absolute path, so an
    /// unchecked `root` is a silent sandbox escape rather than an error.
    #[test]
    fn an_escaping_root_is_rejected_at_parse() {
        for bad in ["/etc", "/", "../sibling", "nested/../../out"] {
            let err = match check_root(bad) {
                Ok(()) => panic!("`root = {bad:?}` escapes the sandbox and must be rejected"),
                Err(e) => format!("{e:#}"),
            };
            assert!(err.contains("root"), "{err}");
            assert!(err.contains("sandbox"), "{err}");
        }
    }

    #[test]
    fn a_relative_root_is_accepted() {
        for ok in ["", "tools", "tools/devenv", "./tools"] {
            check_root(ok).unwrap_or_else(|e| panic!("`root = {ok:?}` should be fine: {e:#}"));
        }
    }

    #[test]
    fn from_options_reads_the_binary_and_rejects_unknown_keys() {
        let mut opts = hplugin::config::Options::new();
        opts.insert(
            "bin".to_string(),
            serde_yaml::from_str("\"/nix/store/x/bin/devenv\"").expect("yaml"),
        );
        let d = Driver::from_options(&opts).expect("options");
        assert_eq!(d.devenv_bin, "/nix/store/x/bin/devenv");

        let mut bad = hplugin::config::Options::new();
        bad.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        Driver::from_options(&bad).expect_err("unknown key must be rejected");
    }
}
