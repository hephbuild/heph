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
//!
//! # Keeping what `runner.json` names alive
//!
//! A `wrap` runner's `runner.json` names this host's nix store paths, and it is
//! cached. Nothing roots them: devenv's own root, `.devenv/gc/shell`, lives in
//! the runner target's sandbox and dangles once the sandbox is deleted, so any
//! `nix-collect-garbage` can delete what a cached `runner.json` hands every
//! consumer, and they fail with exit 127 while the cache keeps serving it.
//!
//! So each run roots **one** store path whose closure covers every store path
//! `runner.json` names, under `<home>/devenv-runner/gcroots/<addr hash>`, and
//! declares that one path in `host_paths`. The engine checks it on every local
//! hit and re-runs the runner when it is gone (see
//! [`root_store_paths`](Driver::root_store_paths) for which path, and why none
//! of devenv's own outputs would do). None of it reaches a hash: the root, its
//! store path and the `nix` that made it are outside the def hash, and
//! `runner.json` does not change.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hdriver_support::run_log::{RunLog, Streams};
use hexecrunner::RunnerRef;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, HostPath,
    ParseRequest, ParseResponse, TargetAddr,
};
use hplugin::error::ProcessFailed;
use hplugin::htspec::{FromSpecValue, Spec};
use hproc::proc_exec;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

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

/// The nix store, where every path a run roots lives.
const NIX_STORE_DIR: &str = "/nix/store";

/// This driver's directory under heph's home, and the gcroots inside it.
const HOME_SUBDIR: &str = "devenv-runner";
const GCROOTS_SUBDIR: &str = "gcroots";

/// The name of the store path a run roots.
const ROOT_NAME: &str = "heph-devenv-env";

/// The expression file the root is built from, in the runner's sandbox.
const ROOT_EXPR_FILE: &str = "heph-devenv-env.nix";

/// The alphabet of a store path's hash part: nix's base32, which has no `e`,
/// `o`, `t` or `u`. A dev environment's output placeholders
/// (`/nix/store/eeee…-name`) are not drawn from it, so they never match.
const NIX32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// The length of a store path's hash part.
const STORE_HASH_LEN: usize = 32;

/// Whether nix allows `b` in a store path's name.
fn is_store_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"+-._?=".contains(&b)
}

/// Every store path `text` names under `store_dir`, as its top-level
/// `<store_dir>/<hash>-<name>`, each once.
///
/// A store path ends where its name does, and the name runs as far as the bytes
/// nix allows in one. A path followed by a byte nix also allows in a name is
/// therefore read too long — `-fmacro-prefix-map=<path>=<other>` reads as
/// `<path>=` — and [`resolve_store_paths`] trims it against the store.
fn store_path_tokens<'a>(text: &'a str, store_dir: &str) -> BTreeSet<&'a str> {
    let prefix = format!("{store_dir}/");
    let bytes = text.as_bytes();
    text.match_indices(prefix.as_str())
        .filter_map(|(start, _)| {
            let hash_start = start + prefix.len();
            let name_start = hash_start + STORE_HASH_LEN + 1;
            let hash = bytes.get(hash_start..hash_start + STORE_HASH_LEN)?;
            if !hash.iter().all(|b| NIX32.contains(b)) || bytes.get(name_start - 1) != Some(&b'-') {
                return None;
            }
            let name_len = bytes
                .get(name_start..)?
                .iter()
                .take_while(|b| is_store_name_byte(**b))
                .count();
            if name_len == 0 {
                return None;
            }
            text.get(start..name_start + name_len)
        })
        .collect()
}

/// The store paths among `tokens` that exist, each trimmed to the path it names.
///
/// A token that does not exist is retried without each `=`-suffix of its name,
/// longest first, which undoes [`store_path_tokens`]'s over-long read. One that
/// still does not exist is left out: a path that is not in the store now cannot
/// go missing from it, and nix refuses to root it.
fn resolve_store_paths(tokens: Vec<String>, store_dir_len: usize) -> BTreeSet<String> {
    // `<store_dir>/<hash>-` and one byte of name: an `=` before it leaves no name.
    let min_len = store_dir_len + 1 + STORE_HASH_LEN + 2;
    tokens
        .into_iter()
        .filter_map(|token| {
            let cuts = token
                .bytes()
                .enumerate()
                .filter(|&(i, b)| b == b'=' && i >= min_len)
                .map(|(i, _)| i)
                .rev();
            let found = std::iter::once(token.len()).chain(cuts).find_map(|end| {
                let path = token.get(..end)?;
                std::fs::symlink_metadata(path).is_ok().then_some(end)
            });
            match found {
                Some(end) => {
                    let mut token = token;
                    token.truncate(end);
                    Some(token)
                }
                None => {
                    tracing::debug!(
                        path = %token,
                        "devenv_runner: runner.json names a store path that is not in the store; \
                         nothing to root"
                    );
                    None
                }
            }
        })
        .collect()
}

/// The nix expression for the root: a store path whose references, and so
/// whose closure, are `paths`.
///
/// `builtins.toFile` records a reference for every store path in its text's
/// context, and `builtins.storePath` puts a path in that context. The paths are
/// embedded literally: [`store_path_tokens`] admits only nix32 and the bytes
/// nix allows in a name, none of which a nix string escapes.
fn root_expr(paths: &BTreeSet<String>) -> String {
    let mut expr = format!(
        "builtins.toFile \"{ROOT_NAME}\" (builtins.concatStringsSep \"\\n\" (map builtins.storePath [\n"
    );
    for path in paths {
        expr.push_str("  \"");
        expr.push_str(path);
        expr.push_str("\"\n");
    }
    expr.push_str("]))\n");
    expr
}

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
    pub mode: Mode,
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
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu. Never hashed.
    pub resources: hplugin::driver::Resources,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    /// The default, for an absent `mode` and for `""`.
    #[default]
    Wrap,
    Session,
}

/// Decoded by hand rather than `#[derive(SpecEnum)]`: `""` is accepted as the
/// default, and an unknown mode is told what each one does. The schema comes
/// from the same table as the parse.
impl FromSpecValue for Mode {
    fn from_spec_value(v: &hcore::htvalue::Value) -> anyhow::Result<Self> {
        Self::parse(&String::from_spec_value(v)?)
    }

    /// What [`Mode::parse`] takes: every mode, and `""`.
    fn spec_param_type() -> ParamType {
        ParamType::one_of(Self::ALL.iter().map(|m| m.as_str()).chain([""]))
    }
}

impl Mode {
    /// Every mode; [`parse`](Self::parse) matches against their
    /// [`as_str`](Self::as_str) spellings.
    const ALL: [Self; 2] = [Self::Wrap, Self::Session];

    fn parse(raw: &str) -> anyhow::Result<Self> {
        // `""` is the default, as an absent `mode` is.
        if raw.is_empty() {
            return Ok(Self::Wrap);
        }
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == raw)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "devenv_runner: unknown mode {raw:?}; expected \"wrap\" (capture the \
                     environment, run targets locally in it) or \"session\" (hold one `devenv \
                     shell` open and run targets inside it)"
                )
            })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Wrap => "wrap",
            Self::Session => "session",
        }
    }
}

#[derive(Debug)]
pub struct Driver {
    /// The `devenv` binary. From the driver's `bin:` option so a workspace can
    /// pin it; `devenv` off `PATH` otherwise.
    devenv_bin: String,
    /// The `nix` that roots the environment, found on the runner's `pass_env`
    /// `PATH` as `devenv` is. Never hashed: it decides what is kept alive, not
    /// what `runner.json` says.
    nix_bin: String,
    /// `<home>/devenv-runner/gcroots`, one root per runner address.
    gcroots_dir: PathBuf,
    /// The store `runner.json` is scanned for; [`NIX_STORE_DIR`] outside tests.
    store_dir: String,
}

impl Driver {
    /// A driver keeping its gcroots under heph's `home`.
    pub fn new(home: &Path) -> Self {
        Self {
            devenv_bin: "devenv".to_string(),
            nix_bin: "nix".to_string(),
            gcroots_dir: home.join(HOME_SUBDIR).join(GCROOTS_SUBDIR),
            store_dir: NIX_STORE_DIR.to_string(),
        }
    }

    pub fn from_options(opts: &hplugin::config::Options, home: &Path) -> anyhow::Result<Self> {
        hplugin::config::deny_unknown("devenv_runner driver", opts, &["bin"])?;
        let bin: Option<String> = hplugin::config::decode_opt(opts, "devenv_runner driver", "bin")?;
        let mut driver = Self::new(home);
        if let Some(bin) = bin.filter(|b| !b.is_empty()) {
            driver.devenv_bin = bin;
        }
        Ok(driver)
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "runner".to_string(),
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
        let mode = spec.mode;
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
                // Local yes, remote never. The captured environment names this
                // machine's nix store paths; publishing it would let one host's
                // answer key another's builds, which is the cross-machine
                // mix-up the fingerprint exists to prevent.
                cache: CacheConfig::on(false),
                hash,
                resources: spec.resources,
                ..Default::default()
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<DevenvDef>().clone();
        // Under `log_capture` the request's writers are the run's log. An older
        // host gets no log from this driver, as before.
        let mut log = req
            .request
            .log_capture
            .then(|| RunLog::from_request(&mut req.request, "log.txt"));

        // The devenv root, inside the sandbox: the declared deps were
        // materialized here, so this is the copy whose content the build
        // depends on.
        let root = if def.root.is_empty() {
            req.sandbox_pkg_dir.clone()
        } else {
            req.sandbox_pkg_dir.join(&def.root)
        };

        let captured = self
            .capture_env(&root, &def, log.as_mut(), ctoken)
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

        let bytes = serde_json::to_vec_pretty(&doc).context("encoding runner.json")?;

        // Rooted before the file is written, so a run that cannot keep what the
        // file names alive leaves no file behind.
        let host_paths = self
            .root_store_paths(
                &bytes,
                &def,
                &req.request.target.addr.hash_str(),
                &req.sandbox_dir,
                log.as_mut(),
                ctoken,
            )
            .await
            .with_context(|| {
                format!(
                    "rooting the nix store paths {OUT_FILE} names, so `nix-collect-garbage` \
                     cannot delete them while the runner stays cached. `{}` is looked up on \
                     the runner's `pass_env` PATH, as `{}` is.",
                    self.nix_bin, self.devenv_bin
                )
            })?;

        let out = req.sandbox_pkg_dir.join(OUT_FILE);
        tokio::fs::write(&out, &bytes)
            .await
            .with_context(|| format!("write {out:?}"))?;

        Ok(ManagedRunResponse {
            host_paths,
            artifacts: vec![],
        })
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
    ///
    /// Under `log_capture` (`log` is `Some`) devenv's stderr — building the
    /// shell — reaches the run's log as it is written, and a failure is the exit
    /// status with no log path. Its stdout is the environment: data, never the
    /// log on either host (it can hold any `pass_env` value). An older host
    /// keeps no log, as before.
    async fn capture_env(
        &self,
        root: &std::path::Path,
        def: &DevenvDef,
        log: Option<&mut RunLog<'_>>,
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

        let stdout = run_tool(
            spec,
            log,
            ctoken,
            || format!("run `{} shell -- env -0`", self.devenv_bin),
            || format!("`{} shell` failed in {root:?}", self.devenv_bin),
        )
        .await?;
        Ok(parse_env0(&stdout))
    }

    /// Root one store path whose closure covers every store path `doc` (the
    /// `runner.json` bytes) names, and return it to be declared.
    ///
    /// # Which path
    ///
    /// A path this run builds itself: `builtins.toFile` over
    /// `builtins.storePath` of every store path `doc` names, so its references
    /// are exactly those paths and its closure covers them by construction.
    /// `nix build --out-link` writes and roots it in one step, so it is never
    /// unrooted for a garbage collection to take.
    ///
    /// None of devenv's own outputs covers what a captured environment names
    /// (measured on devenv 2.4):
    ///
    /// - `DEVENV_PROFILE` is the `packages` buildEnv. Its closure misses what
    ///   the shell sets outside `packages`: `stdenv`, a `RUST_SRC_PATH`, the
    ///   `-env` JSON.
    /// - `.devenv/gc/shell` (the `mkShell` output, devenv's own root) misses the
    ///   `-env` JSON that `out` names.
    /// - The `-env` JSON misses the `PATH` entries devenv's wrapper prepends for
    ///   its own closure (`devenv`, `cachix`, `nixd`).
    /// - None covers a store path that reaches the capture through `pass_env`,
    ///   such as a `PATH` inherited from an outer nix shell.
    ///
    /// Each is also a devenv implementation detail that has moved between
    /// versions; the root depends only on what `runner.json` says.
    ///
    /// # What is scanned
    ///
    /// The output, not the capture: a `session` runner's file names its launch
    /// argv and the real tree, not the environment, so it roots only a `bin:`
    /// that is a store path. A file that names no store path roots nothing,
    /// declares nothing, and runs no `nix`.
    async fn root_store_paths(
        &self,
        doc: &[u8],
        def: &DevenvDef,
        addr_hash: &str,
        sandbox_dir: &Path,
        log: Option<&mut RunLog<'_>>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Vec<HostPath>> {
        let text = std::str::from_utf8(doc).context("runner.json is not UTF-8")?;
        let tokens: Vec<String> = store_path_tokens(text, &self.store_dir)
            .into_iter()
            .map(str::to_owned)
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let store_dir_len = self.store_dir.len();
        let paths = hcore::blocking::run(move || resolve_store_paths(tokens, store_dir_len)).await;
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        tokio::fs::create_dir_all(&self.gcroots_dir)
            .await
            .with_context(|| format!("create gcroots dir {:?}", self.gcroots_dir))?;
        // One root per address: a run replaces the root its last run made, and
        // an entry whose path a GC then takes re-runs on its next hit.
        let gcroot = self.gcroots_dir.join(addr_hash);
        // `nix build --out-link` refuses to overwrite an existing path.
        match tokio::fs::remove_file(&gcroot).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove stale gcroot {gcroot:?}")),
        }

        // A file, not `--expr`: an environment can name hundreds of paths, and
        // Linux caps one argv string at 128 KiB.
        let expr_file = sandbox_dir.join(ROOT_EXPR_FILE);
        tokio::fs::write(&expr_file, root_expr(&paths))
            .await
            .with_context(|| format!("write {expr_file:?}"))?;

        let spec = proc_exec::Spec {
            program: PathBuf::from(&self.nix_bin),
            args: vec![
                OsString::from("--extra-experimental-features"),
                OsString::from("nix-command"),
                OsString::from("build"),
                // `builtins.storePath` is refused under pure evaluation.
                OsString::from("--impure"),
                OsString::from("--file"),
                expr_file.into_os_string(),
                OsString::from("--out-link"),
                gcroot.clone().into_os_string(),
                OsString::from("--print-out-paths"),
            ],
            // The hashed snapshot devenv ran under, so `nix` is the one on the
            // runner's PATH and talks to the same store.
            env: def
                .pass_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            cwd: sandbox_dir.to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: true,
            ctty: false,
        };
        let stdout = run_tool(
            spec,
            log,
            ctoken,
            || format!("run `{} build` to root the environment", self.nix_bin),
            || {
                format!(
                    "`{} build` failed rooting {} store paths",
                    self.nix_bin,
                    paths.len()
                )
            },
        )
        .await
        .map_err(|e| {
            let missing = e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            });
            if missing {
                e.context(format!(
                    "`{}` was not found. A devenv runner whose output names nix store paths \
                     needs `nix` on its `pass_env` PATH to root them",
                    self.nix_bin
                ))
            } else {
                e
            }
        })?;

        let printed = std::str::from_utf8(&stdout).context("nix build stdout is not UTF-8")?;
        let mut lines = printed.lines().filter(|l| !l.is_empty());
        let (Some(root), None) = (lines.next(), lines.next()) else {
            anyhow::bail!("nix build printed {printed:?}, expected exactly one store path");
        };
        anyhow::ensure!(
            root.strip_prefix(&self.store_dir)
                .is_some_and(|rest| rest.starts_with('/')),
            "nix build printed {root:?}, which is not under {}",
            self.store_dir
        );
        tracing::debug!(
            root,
            gcroot = %gcroot.display(),
            paths = paths.len(),
            "devenv_runner: rooted the store paths runner.json names"
        );
        Ok(vec![HostPath::new(root)?])
    }
}

/// Run a tool the driver needs and return its stdout.
///
/// Under `log_capture` (`log` is `Some`) its stderr reaches the run's log as it
/// is written, and a failure is the exit status with no log path. Its stdout is
/// data, never the log. Without a log, a failure carries stderr in its message.
async fn run_tool(
    spec: proc_exec::Spec,
    log: Option<&mut RunLog<'_>>,
    ctoken: &(dyn Cancellable + Send + Sync),
    spawn_what: impl FnOnce() -> String,
    fail_what: impl FnOnce() -> String,
) -> anyhow::Result<Vec<u8>> {
    let captured = log.is_some();
    let out = match log {
        Some(log) => {
            log.output(RunnerRef::local(), spec, Streams::STDERR, ctoken)
                .await
        }
        None => hexecrunner::output(RunnerRef::local(), spec, ctoken).await,
    }
    .with_context(spawn_what)?;
    if !out.status.success() {
        let what = fail_what();
        if captured {
            return Err(anyhow::Error::new(ProcessFailed::from_status(out.status)).context(what));
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{what}: {stderr}");
    }
    Ok(out.stdout)
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

    /// The schema lists exactly what `Mode::parse` takes — `""` included, since
    /// the parser takes it — so the LSP never flags a valid mode; an absent
    /// `mode` is the default.
    #[test]
    fn mode_schema_is_the_literals_parse_takes() {
        let absent = TargetSpec::from(&Default::default()).expect("absent mode");
        assert_eq!(absent.mode, Mode::Wrap);
        let schema = TargetSpec::schema();
        let ty = &schema
            .fields
            .iter()
            .find(|f| f.name == "mode")
            .expect("mode field")
            .ty;
        assert_eq!(ty.literals(), vec!["wrap", "session", ""]);
        for l in ty.literals() {
            Mode::parse(l).unwrap_or_else(|e| panic!("schema lists {l:?}: {e:#}"));
        }
        assert!(!ty.matches(&hcore::htvalue::Value::String("agent".to_string())));
        Mode::parse("agent").expect_err("near-miss refused");
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
        let d = Driver::new(Path::new("/home"));

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
        let cfg = Driver::new(Path::new("/home")).runner_config(
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
        let d = Driver::from_options(&opts, Path::new("/home")).expect("options");
        assert_eq!(d.devenv_bin, "/nix/store/x/bin/devenv");
        assert_eq!(d.gcroots_dir, Path::new("/home/devenv-runner/gcroots"));

        let mut bad = hplugin::config::Options::new();
        bad.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        Driver::from_options(&bad, Path::new("/home")).expect_err("unknown key must be rejected");
    }
}

/// Where devenv's output goes: its stderr is the run's log, its stdout (the
/// environment) is data and never the log.
#[cfg(test)]
mod log_tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hplugin::driver::RunRequest;
    use std::sync::Arc;

    /// Write an executable `/bin/sh` script running `body` at `path`.
    pub(super) fn script(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir script dir");
        }
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// A stand-in `devenv` running `body` under `/bin/sh`, with heph's home at
    /// `dir/home`, the store at `dir/store` and `nix` at `dir/nix` (which only a
    /// test that writes it has).
    pub(super) fn fake(dir: &std::path::Path, body: &str) -> Driver {
        let bin = dir.join("devenv");
        script(&bin, body);
        let mut driver = Driver::new(&dir.join("home"));
        driver.devenv_bin = bin.to_string_lossy().into_owned();
        driver.nix_bin = dir.join("nix").to_string_lossy().into_owned();
        driver.store_dir = dir.join("store").to_string_lossy().into_owned();
        driver
    }

    /// The runner target `//pkg:env`, parsed, with `mode` if one is given.
    pub(super) async fn parse(driver: &Driver, mode: Option<&str>) -> TargetDef {
        let config = mode
            .map(|m| {
                (
                    "mode".to_string(),
                    hcore::htvalue::Value::String(m.to_string()),
                )
            })
            .into_iter()
            .collect();
        let parse = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: hmodel::htaddr::parse_addr("//pkg:env").expect("addr"),
                driver: "devenv_runner".to_string(),
                config,
                ..Default::default()
            }),
        };
        driver
            .parse(parse, &StdCancellationToken::new())
            .await
            .expect("parse")
            .target_def
    }

    /// Run `driver`; returns the result and what reached the request's stdout
    /// and stderr writers.
    pub(super) async fn run(
        driver: &Driver,
        dir: &std::path::Path,
        log_capture: bool,
    ) -> (anyhow::Result<ManagedRunResponse>, String, String) {
        run_mode(driver, dir, log_capture, None).await
    }

    /// [`run`], in `mode`.
    pub(super) async fn run_mode(
        driver: &Driver,
        dir: &std::path::Path,
        log_capture: bool,
        mode: Option<&str>,
    ) -> (anyhow::Result<ManagedRunResponse>, String, String) {
        let ctoken = StdCancellationToken::new();
        let def = parse(driver, mode).await;
        let id = "test".to_string();
        let sandbox = dir.join("sandbox");
        let pkg = sandbox.join("ws/pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir pkg");
        let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
        let mut request = RunRequest::new(&id, &def, &sandbox);
        request.tree_root_path = dir.join("ws");
        request.log_capture = log_capture;
        request.stdout = Some(&mut out);
        request.stderr = Some(&mut err);
        let req = ManagedRunRequest {
            request,
            sandbox_dir: sandbox.clone(),
            sandbox_ws_dir: sandbox.join("ws"),
            sandbox_pkg_dir: pkg,
            inputs: vec![],
        };
        let res = driver.run(req, &ctoken).await;
        (
            res,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    /// Under `log_capture` building the shell reaches the log; the environment
    /// (stdout, which may hold a secret) never does, and still becomes the
    /// runner config.
    #[tokio::test]
    async fn devenv_progress_reaches_the_log_and_the_environment_never_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = fake(
            dir.path(),
            "echo 'building shell' >&2; printf 'APP_CHANNEL=stable\\0TOKEN=env-secret-value\\0'",
        );
        let (res, out, err) = run(&driver, dir.path(), true).await;
        let res = res.expect("run");
        assert!(res.artifacts.is_empty(), "no Log artifact crosses");
        assert_eq!(err, "building shell\n");
        assert_eq!(out, "", "the environment is data, never the log");
        let doc = std::fs::read_to_string(dir.path().join("sandbox/ws/pkg").join(OUT_FILE))
            .expect("runner.json");
        assert!(
            doc.contains("env-secret-value"),
            "the capture still lands: {doc}"
        );
        assert!(!dir.path().join("sandbox/log.txt").exists());
    }

    /// A failed shell under `log_capture`: devenv's stderr is in the log, and
    /// the error is the exit status, naming no log path and not repeating it.
    /// An older host keeps today's error, with stderr in the message, and its
    /// writers see nothing.
    #[tokio::test]
    async fn a_failed_shell_is_logged_under_log_capture_and_unchanged_without() {
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = fake(
            dir.path(),
            "echo 'error: devenv.nix is broken' >&2; printf 'PARTIAL=env\\0'; exit 1",
        );
        let (res, out, err) = run(&driver, dir.path(), true).await;
        let Err(e) = res else {
            panic!("the shell fails")
        };
        assert!(
            e.chain()
                .any(|c| c.downcast_ref::<ProcessFailed>().is_some()),
            "{e:#}"
        );
        assert!(hplugin::error::failure_log(&e).is_none(), "{e:#}");
        let msg = format!("{e:#}");
        assert!(msg.contains("shell` failed in"), "{msg}");
        assert!(!msg.contains("devenv.nix is broken"), "{msg}");
        assert_eq!(err, "error: devenv.nix is broken\n");
        assert_eq!(out, "", "even a failed shell's stdout is never the log");

        let (res, out, err) = run(&driver, dir.path(), false).await;
        let Err(e) = res else {
            panic!("the shell fails")
        };
        let msg = format!("{e:#}");
        assert!(
            msg.contains("failed in") && msg.contains("devenv.nix is broken"),
            "{msg}"
        );
        assert_eq!((out.as_str(), err.as_str()), ("", ""));
    }
}

/// The run roots one store path covering every store path `runner.json` names,
/// and declares exactly that one. A fake `nix` stands in for the real one: it
/// writes the root as `builtins.toFile` would (one line per reference) and
/// makes the out-link, so the root's references are readable back.
#[cfg(test)]
mod root_tests {
    use super::log_tests::{fake, parse, run, run_mode, script};
    use super::*;

    /// A valid hash part: nix32 rotated by `n`.
    fn hash(n: usize) -> String {
        let mut h = NIX32.to_vec();
        h.rotate_left(n % NIX32.len());
        String::from_utf8(h).expect("ascii")
    }

    /// The store path the fake `nix` writes the root to.
    fn root_path(dir: &Path) -> String {
        format!("{}/{}-{ROOT_NAME}", dir.join("store").display(), hash(31))
    }

    /// A fake `nix` at `dir/nix` that records its argv in `dir/nix.args`, writes
    /// the root with the expression's store paths as its references, and links
    /// it at `--out-link`. `ln -s` fails on an existing link, as `nix build
    /// --out-link` refuses one.
    fn fake_nix(dir: &Path) {
        fake_nix_rooting_in(dir, dir);
    }

    /// [`fake_nix`] at `dir`, writing the root into `store_owner`'s store.
    fn fake_nix_rooting_in(dir: &Path, store_owner: &Path) {
        let root = root_path(store_owner);
        let args = dir.join("nix.args");
        script(
            &dir.join("nix"),
            &format!(
                r#"printf '%s\n' "$@" > '{args}'
file=; link=
while [ $# -gt 0 ]; do
  case "$1" in
    --file) file=$2; shift;;
    --out-link) link=$2; shift;;
  esac
  shift
done
grep '^  "' "$file" | tr -d ' "' > '{root}' || exit 1
ln -s '{root}' "$link" || exit 1
echo '{root}'"#,
                args = args.display(),
            ),
        );
    }

    /// The root's references, as the fake `nix` recorded them.
    fn references(root: &str) -> BTreeSet<String> {
        std::fs::read_to_string(root)
            .expect("read root")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A store path under `dir/store` that exists, as a directory.
    fn store_dir_path(dir: &Path, n: usize, name: &str) -> String {
        let p = format!("{}/{}-{name}", dir.join("store").display(), hash(n));
        std::fs::create_dir_all(&p).expect("mkdir store path");
        p
    }

    /// A fake `devenv` body printing `env` as `env -0` would.
    fn print_env(env: &[(&str, String)]) -> String {
        let records: String = env.iter().map(|(k, v)| format!("{k}={v}\\0")).collect();
        format!("printf '{records}'")
    }

    /// A captured environment naming store paths everywhere a real one does: a
    /// `PATH` with `/bin` suffixes and a repeat, `-isystem`, a
    /// `-fmacro-prefix-map=<path>=<placeholder>` whose path is followed by a
    /// byte nix allows in a name, the profile, a file, a path that is not in
    /// the store, and a placeholder. Returns the body and the paths that exist.
    fn env_naming_store_paths(dir: &Path) -> (String, BTreeSet<String>) {
        let bash = store_dir_path(dir, 0, "bash-5.3");
        let cc = store_dir_path(dir, 1, "clang-wrapper-21");
        let libc = store_dir_path(dir, 2, "libcxx-21-dev");
        let python = store_dir_path(dir, 3, "python3-3.14.8");
        let profile = store_dir_path(dir, 4, "devenv-profile");
        let json = format!(
            "{}/{}-devenv-shell-env",
            dir.join("store").display(),
            hash(5)
        );
        std::fs::write(&json, "{}").expect("write a store file");
        let store = dir.join("store").display().to_string();
        let gone = format!("{store}/{}-collected-1.0", hash(6));
        let placeholder = format!("{store}/{}-out", "e".repeat(STORE_HASH_LEN));
        let body = print_env(&[
            ("PATH", format!("{bash}/bin:{cc}/bin:/usr/bin:{bash}/bin")),
            (
                "NIX_CFLAGS_COMPILE",
                format!("-isystem {libc}/include -fmacro-prefix-map={python}={placeholder}"),
            ),
            ("DEVENV_PROFILE", profile.clone()),
            ("out", json.clone()),
            ("STALE", format!("{gone}/lib")),
        ]);
        let exist = [bash, cc, libc, python, profile, json]
            .into_iter()
            .collect();
        (body, exist)
    }

    /// The covering path's closure holds every store path `runner.json` names,
    /// and nothing it does not: each existing path once, an over-long read
    /// trimmed back to the path, the path that is not in the store and the
    /// placeholder left out.
    #[tokio::test]
    async fn every_store_path_runner_json_names_is_in_the_declared_closure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (body, exist) = env_naming_store_paths(dir.path());
        let driver = fake(dir.path(), &body);
        fake_nix(dir.path());

        let (res, _, _) = run(&driver, dir.path(), false).await;
        let res = res.expect("run");
        let [root] = res.host_paths.as_slice() else {
            panic!("exactly one declared path: {:?}", res.host_paths)
        };
        let mut closure = references(root.as_str());
        closure.insert(root.as_str().to_owned());

        let doc = std::fs::read_to_string(dir.path().join("sandbox/ws/pkg").join(OUT_FILE))
            .expect("runner.json");
        for path in &exist {
            assert!(doc.contains(path.as_str()), "the fixture names {path}");
            assert!(
                closure.contains(path),
                "{path} is named but outside the closure"
            );
        }
        assert_eq!(
            references(root.as_str()),
            exist,
            "the root references exactly what runner.json names and the store has"
        );
    }

    /// The declared path is the one `nix build` printed and rooted, and the root
    /// is a link to it at `<home>/devenv-runner/gcroots/<addr hash>`. A link an
    /// earlier run left there is replaced, not refused.
    #[tokio::test]
    async fn declares_exactly_the_covering_path_and_roots_it_under_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (body, _) = env_naming_store_paths(dir.path());
        let driver = fake(dir.path(), &body);
        fake_nix(dir.path());
        let addr_hash = hmodel::htaddr::parse_addr("//pkg:env")
            .expect("addr")
            .hash_str();
        let gcroot = dir
            .path()
            .join("home/devenv-runner/gcroots")
            .join(&addr_hash);
        std::fs::create_dir_all(gcroot.parent().expect("parent")).expect("mkdir gcroots");
        std::os::unix::fs::symlink("/nix/store/an-earlier-run", &gcroot).expect("stale gcroot");

        let (res, _, _) = run(&driver, dir.path(), false).await;
        let res = res.expect("run");

        let root = root_path(dir.path());
        assert_eq!(
            res.host_paths
                .iter()
                .map(HostPath::as_str)
                .collect::<Vec<_>>(),
            [root.as_str()]
        );
        assert_eq!(
            std::fs::read_link(&gcroot).expect("gcroot"),
            Path::new(&root),
            "the gcroot links the declared path"
        );
        let args = std::fs::read_to_string(dir.path().join("nix.args")).expect("nix ran");
        let args: Vec<&str> = args.lines().collect();
        for want in ["build", "--impure", "--print-out-paths"] {
            assert!(args.contains(&want), "{want} in {args:?}");
        }
        let link = args
            .iter()
            .position(|a| *a == "--out-link")
            .and_then(|i| args.get(i + 1));
        assert_eq!(link, Some(&gcroot.to_str().expect("utf-8")));
    }

    /// A `runner.json` naming no store path costs nothing: no `nix`, no gcroot,
    /// nothing declared.
    #[tokio::test]
    async fn an_output_naming_no_store_path_roots_nothing_and_runs_no_nix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let driver = fake(dir.path(), "printf 'APP_CHANNEL=stable\\0PATH=/usr/bin\\0'");
        fake_nix(dir.path());

        let (res, _, _) = run(&driver, dir.path(), false).await;
        assert!(res.expect("run").host_paths.is_empty());
        assert!(!dir.path().join("nix.args").exists(), "nix never ran");
        assert!(!dir.path().join("home/devenv-runner").exists());
    }

    /// A `session` runner's file names its launch, not the environment: the
    /// environment's store paths are not rooted, and a `bin:` in the store is.
    #[tokio::test]
    async fn a_session_runner_roots_what_its_launch_names_not_the_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (body, _) = env_naming_store_paths(dir.path());
        let mut driver = fake(dir.path(), &body);
        fake_nix(dir.path());

        let (res, _, _) = run_mode(&driver, dir.path(), false, Some("session")).await;
        assert!(res.expect("run").host_paths.is_empty());
        assert!(!dir.path().join("nix.args").exists(), "nix never ran");

        let devenv = store_dir_path(dir.path(), 7, "devenv-2.4.0");
        let bin = format!("{devenv}/bin/devenv");
        script(Path::new(&bin), &body);
        driver.devenv_bin = bin;
        let (res, _, _) = run_mode(&driver, dir.path(), false, Some("session")).await;
        let res = res.expect("run");
        let [root] = res.host_paths.as_slice() else {
            panic!("exactly one declared path: {:?}", res.host_paths)
        };
        assert_eq!(references(root.as_str()), BTreeSet::from([devenv]));
    }

    /// I1: rooting reaches no hash. The def hash and the `runner.json` bytes are
    /// the same whether a run roots or not, whichever home it roots under and
    /// whichever `nix` does it.
    #[tokio::test]
    async fn rooting_reaches_neither_the_def_hash_nor_runner_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (body, _) = env_naming_store_paths(dir.path());
        let rooting = fake(dir.path(), &body);
        fake_nix(dir.path());

        let other = tempfile::tempdir().expect("tempdir");
        let mut elsewhere = fake(other.path(), &body);
        // The same `devenv` and store, through another `nix` under another home.
        // (`bin:` is hashed, rightly: it decides what the capture is.)
        elsewhere.devenv_bin = rooting.devenv_bin.clone();
        elsewhere.store_dir = rooting.store_dir.clone();
        fake_nix_rooting_in(other.path(), dir.path());

        let mut not_rooting = fake(dir.path(), &body);
        not_rooting.store_dir = "/nowhere".to_string();

        let doc =
            |d: &Path| std::fs::read(d.join("sandbox/ws/pkg").join(OUT_FILE)).expect("runner.json");
        let (res, _, _) = run(&rooting, dir.path(), false).await;
        assert_eq!(res.expect("run").host_paths.len(), 1);
        let rooted = doc(dir.path());
        let (res, _, _) = run(&elsewhere, other.path(), false).await;
        assert_eq!(res.expect("run").host_paths.len(), 1);
        assert_eq!(doc(other.path()), rooted);
        let (res, _, _) = run(&not_rooting, dir.path(), false).await;
        assert!(res.expect("run").host_paths.is_empty());
        assert_eq!(doc(dir.path()), rooted);

        let hash = parse(&rooting, None).await.hash;
        assert_eq!(parse(&elsewhere, None).await.hash, hash);
        assert_eq!(parse(&not_rooting, None).await.hash, hash);
    }

    /// A root that cannot be made fails the run with what it was for, and no
    /// `runner.json` is left behind to be cached unrooted.
    #[tokio::test]
    async fn a_failed_root_fails_the_run_and_writes_no_runner_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (body, _) = env_naming_store_paths(dir.path());
        let driver = fake(dir.path(), &body);
        script(
            &dir.path().join("nix"),
            "echo 'error: cannot connect to socket' >&2; exit 1",
        );

        let (res, _, _) = run(&driver, dir.path(), false).await;
        let Err(e) = res else {
            panic!("the root fails")
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("nix-collect-garbage"), "{msg}");
        assert!(msg.contains("cannot connect to socket"), "{msg}");
        assert!(
            !dir.path().join("sandbox/ws/pkg").join(OUT_FILE).exists(),
            "no runner.json"
        );
    }

    /// An output naming store paths with no `nix` to root them fails the run,
    /// saying what was missing and what it was for, and leaves no `runner.json`
    /// to be cached unrooted.
    #[tokio::test]
    async fn missing_nix_fails_with_a_clear_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (body, _) = env_naming_store_paths(dir.path());
        // `fake` points `nix` at `dir/nix`, which nothing writes here.
        let driver = fake(dir.path(), &body);

        let (res, _, _) = run(&driver, dir.path(), false).await;
        let Err(e) = res else {
            panic!("the run fails without nix")
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("pass_env"), "{msg}");
        assert!(msg.contains("nix-collect-garbage"), "{msg}");
        assert!(
            !dir.path().join("sandbox/ws/pkg").join(OUT_FILE).exists(),
            "no runner.json"
        );
    }

    /// The scanner reads a store path only where one is well formed, and reads
    /// its name as far as nix allows.
    #[test]
    fn store_path_tokens_read_only_well_formed_store_paths() {
        let h = hash(0);
        let s = "/nix/store";
        let good = format!("{s}/{h}-bash-5.3");
        let cases: Vec<(String, Vec<String>)> = vec![
            (good.clone(), vec![good.clone()]),
            (format!("{good}/bin/bash"), vec![good.clone()]),
            (format!("x:{good}:y"), vec![good.clone()]),
            (format!("\"{good}\""), vec![good.clone()]),
            (format!("{good} {good}"), vec![good.clone()]),
            (format!("{good}=/x"), vec![format!("{good}=")]),
            (format!("{good}é"), vec![good.clone()]),
            (
                format!("{s}/{h}-a+b?c=d_e.f"),
                vec![format!("{s}/{h}-a+b?c=d_e.f")],
            ),
            // Not a store path.
            (format!("{s}X/{h}-bash"), vec![]),
            (format!("/nix/stor/{h}-bash"), vec![]),
            (format!("{s}/{}-bash", "e".repeat(STORE_HASH_LEN)), vec![]),
            (format!("{s}/{}-bash", h.get(..31).expect("31")), vec![]),
            (format!("{s}/{h}bash"), vec![]),
            (format!("{s}/{h}-"), vec![]),
            (format!("{s}/{h}-/bin"), vec![]),
            (format!("{s}/{h}"), vec![]),
            (s.to_string(), vec![]),
            (String::new(), vec![]),
        ];
        for (text, want) in cases {
            let got: Vec<&str> = store_path_tokens(&text, s).into_iter().collect();
            assert_eq!(got, want, "scanning {text:?}");
        }
    }

    /// An over-long read is trimmed back to the path the store has, a name that
    /// really holds `=` is kept whole, and a path the store does not have is
    /// left out.
    #[test]
    fn resolving_trims_an_over_long_read_and_drops_what_the_store_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = store_dir_path(dir.path(), 0, "python3-3.14.8");
        let with_eq = store_dir_path(dir.path(), 1, "a=b");
        let store_len = dir.path().join("store").to_string_lossy().len();
        let gone = format!("{}/{}-gone", dir.path().join("store").display(), hash(2));
        let tokens = vec![
            format!("{plain}="),
            format!("{plain}=x=y"),
            with_eq.clone(),
            format!("{with_eq}="),
            gone,
        ];
        assert_eq!(
            resolve_store_paths(tokens, store_len),
            BTreeSet::from([plain, with_eq])
        );
    }
}
