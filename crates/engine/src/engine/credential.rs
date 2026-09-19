//! Host-side handling of credential references: the chain walk, acquisition, and
//! presentation.
//!
//! A credential is declared as a target (`driver = "credential"`) and referenced
//! by addr from the targets that use it. The reference arrives as an [`Input`]
//! with `hashed: false, runtime: false`, marked by
//! [`CREDENTIAL_ANNOTATION`](hdriver_support::credential::CREDENTIAL_ANNOTATION).
//!
//! # The load-bearing claim
//!
//! **Nothing about a credential enters `hashin`** — not the material, not the
//! chosen source, not the declaration, not even the names of the variables it
//! presents. That is structural rather than conventional: `hashin` is computed
//! over `hashed: true` inputs only, so an edge with both flags false has no path
//! to it. The test that proves it is the same shape as scratch's.
//!
//! **A credential is acquired only on a miss.** Everything in this module hangs
//! off `execute`, which a cache hit never reaches. A fully cached build probes
//! nothing, spawns nothing and prompts nobody.
//!
//! # Where a run's credential files live, and why the path is load-bearing
//!
//! Presentation files are written at `<sandbox_dir>/.heph/auth/<addr>/<name>` — a
//! sibling of the workspace directory, **never inside it**. This is not tidiness.
//! Output collection is rooted at the workspace directory and packs every regular
//! file it walks, and unlike a scratch mount (a symlink to an absolute path, which
//! the artifact packer refuses) a token file is an ordinary file that would pack
//! silently, land in the local cache, and be pushed to the shared remote
//! automatically.
//!
//! Two more rules fall out of the same reasoning. The directory is named by the
//! full sanitized *address* rather than the target name, so two credentials called
//! `aws` in different packages cannot collide. And it is deleted at run end
//! regardless of outcome — a failed target's sandbox is deliberately kept for
//! diagnostics, and the log tail is what makes it useful, not the token.

use crate::engine::Engine;
use crate::engine::driver::targetdef::Input;
use crate::engine::request_state::RequestState;
use anyhow::Context as _;
use hbuiltins::plugincredential::{
    CredentialDef, DRIVER_NAME, Dialect, Presentation, SourceDecl, SourceKind, When,
    parse_declaration, source::detected_ci_provider,
};
use hcore::template::Ref;
use hdriver_support::credential::is_credential;
use hmodel::htaddr::Addr;
use hplugin::driver::{CREDENTIAL_ENV_MAX_BYTES, CredentialMount};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub use crate::engine::credential_store::{CredentialStore, Material};

/// A reference resolved against its declaration.
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub addr: Addr,
    pub def: CredentialDef,
}

/// What led to a credential: the addresses already being acquired, and the
/// resolution keys of the parents whose identity this one is derived under.
///
/// The two halves are deliberately separate because they answer different
/// questions. `visited` is the **recursion guard** — a credential that names
/// itself, directly or through another, must fail rather than deadlock, and it
/// has to be checked in address space. `keys` is the **identity** — material
/// minted under one root must never be served to a run holding another, which is
/// checked in key space, both in the process cache and in the disk store.
///
/// Conflating them is how the guard came to compare a 16-hex resolution key
/// against `"//auth:x"` and therefore never fire.
#[derive(Debug, Clone, Default)]
pub struct Chain {
    visited: Vec<Addr>,
    keys: Vec<String>,
}

impl Chain {
    /// The empty chain: a credential a consumer named directly.
    pub fn root() -> Self {
        Self::default()
    }

    /// `Err` when `addr` is already being acquired further up, naming the loop.
    fn enter(&self, addr: &Addr) -> anyhow::Result<()> {
        if !self.visited.iter().any(|a| a == addr) {
            return Ok(());
        }
        let mut path: Vec<String> = self.visited.iter().map(Addr::format).collect();
        path.push(addr.format());
        anyhow::bail!(
            "credential {addr} is its own source, directly or through another credential: {}. A chain \
             cannot depend on itself — the fix is usually a second, bootstrap credential \
             that reads no state of its own",
            path.join(" → ")
        )
    }

    /// The chain as seen by the sources *of* `addr`.
    fn descend(&self, addr: &Addr) -> Self {
        let mut visited = self.visited.clone();
        visited.push(addr.clone());
        Self {
            visited,
            keys: self.keys.clone(),
        }
    }

    fn with_key(&self, key: String) -> Self {
        let mut keys = self.keys.clone();
        keys.push(key);
        Self {
            visited: self.visited.clone(),
            keys,
        }
    }

    fn keys(&self) -> &[String] {
        &self.keys
    }
}

/// Material plus everything a human needs to know about how it got here.
#[derive(Debug, Clone)]
pub struct Acquired {
    pub material: Material,
    /// The winning source's label, e.g. `exec(aws)`.
    pub source: String,
    /// Index of the winning source in the chain.
    pub source_index: usize,
    pub acquired_at: SystemTime,
    /// The store key this was cached under. A child credential folds its
    /// parents' keys into its own.
    pub key: String,
    /// Whether it came from the disk tier rather than being freshly acquired.
    pub from_disk: bool,
}

/// One line of a chain walk: a source, and what happened to it.
#[derive(Debug, Clone)]
pub struct WalkStep {
    pub index: usize,
    pub label: String,
    /// `None` when this source was chosen.
    pub skipped: Option<String>,
    /// What to tell a human, when this source was skipped.
    pub hint: String,
}

/// Every source was skipped. The commonest failure in CI, and today an
/// unreadable one — so the error prints the walk, with the fix attached to the
/// line that failed.
#[derive(Debug)]
pub struct NoApplicableSourceError {
    pub addr: Addr,
    pub walk: Vec<WalkStep>,
}

impl std::fmt::Display for NoApplicableSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "credential {} — no source applies here", self.addr)?;
        for step in &self.walk {
            let reason = step.skipped.as_deref().unwrap_or("skipped");
            writeln!(
                f,
                "  {}. {:<24} skipped: {reason}",
                step.index + 1,
                step.label
            )?;
            writeln!(f, "{:28}→ {}", "", step.hint)?;
        }
        Ok(())
    }
}

impl std::error::Error for NoApplicableSourceError {}

/// A credential exists here but needs a human to sign in.
///
/// A build never signs anyone in: it fails with the exact command to run, and a
/// human runs it. The `login` argvs travel as structured data rather than as
/// prose so `--json` consumers and the TUI render the same instruction.
#[derive(Debug)]
pub struct AuthRequiredError {
    pub addr: Addr,
    pub source: String,
    pub login: Vec<Vec<String>>,
}

impl std::fmt::Display for AuthRequiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "credential {} needs a sign-in ({}) — run: heph auth login {}",
            self.addr, self.source, self.addr
        )
    }
}

impl std::error::Error for AuthRequiredError {}

impl AuthRequiredError {
    /// This error, with the chain walk that produced it as its cause.
    ///
    /// Both, deliberately: the walk says *why nothing applied*, and this says
    /// *what to run about it*. A `--json` consumer downcasts to this; a human
    /// reads both paragraphs.
    fn context(self, walk: NoApplicableSourceError) -> anyhow::Error {
        anyhow::Error::new(walk).context(self)
    }
}

/// The sign-in commands a source declares, if any.
///
/// Only `exec` and `passthrough` have one: nothing a human can run repairs a
/// missing OIDC endpoint or an unset environment variable.
pub fn source_login(src: &SourceDecl) -> &[Vec<String>] {
    match &src.kind {
        SourceKind::Exec { login, .. } | SourceKind::Passthrough { login, .. } => login,
        _ => &[],
    }
}

/// True when this input is a credential reference rather than an ordinary dep.
///
/// Both flags false *and* the annotation. The flags alone are the scratch shape
/// too, so the annotation is what tells them apart.
pub(crate) fn is_credential_input(i: &Input) -> bool {
    !i.hashed && !i.runtime && is_credential(&i.annotations)
}

/// The per-process acquisition cache.
///
/// Not the engine's [`Memoizer`](hcore::hmemoizer::Memoizer): a memoized cell is
/// computed once and kept forever, and material expires. A three-hour build with
/// a one-hour token would otherwise hand a target starting at hour two material
/// that lapsed an hour earlier. So each key gets an async mutex holding an
/// `Option`, which gives single-flighting *and* re-acquisition on expiry from the
/// same structure.
#[derive(Default)]
pub struct CredentialCache {
    cells: parking_lot::Mutex<
        std::collections::HashMap<String, Arc<tokio::sync::Mutex<Option<Acquired>>>>,
    >,
}

impl CredentialCache {
    /// The cell for one credential *under one root identity*.
    ///
    /// Keyed on the address **and** the parents' resolution keys, not the address
    /// alone. That is the same invariant the disk store's key enforces, and
    /// dropping it here would reintroduce one tier up exactly what the disk key
    /// exists to prevent: a derived credential is reachable both directly from a
    /// consumer and as a source-credential under some parent, and whichever
    /// material landed in the cell first would then serve both for the rest of
    /// the process.
    fn cell(&self, addr: &str, parents: &[String]) -> Arc<tokio::sync::Mutex<Option<Acquired>>> {
        let mut key = String::with_capacity(addr.len() + parents.len() * 17);
        key.push_str(addr);
        for p in parents {
            key.push('\u{1f}');
            key.push_str(p);
        }
        Arc::clone(
            self.cells
                .lock()
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
        )
    }

    fn clear(&self) {
        self.cells.lock().clear();
    }
}

impl Engine {
    /// Resolve every credential reference on a def against its declaration, and
    /// reject an incoherent set.
    ///
    /// Done on the finalized def, like `resolve_scratch`, so every driver gets the
    /// checks and a BUILD-file mistake surfaces at `get_def` — where the author is
    /// still looking at the file — rather than as an identity that quietly does
    /// nothing much later.
    ///
    /// Returns immediately for a target with no references, which is nearly all
    /// of them.
    pub(crate) async fn resolve_credentials(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        inputs: &[Input],
    ) -> anyhow::Result<Vec<ResolvedCredential>> {
        let refs: Vec<&Input> = inputs.iter().filter(|i| is_credential_input(i)).collect();
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        let mut resolved = Vec::with_capacity(refs.len());
        for input in refs {
            let addr = input.r#ref.r#ref.clone();
            let spec = Arc::clone(self)
                .get_spec(rs.clone(), &addr)
                .await
                .with_context(|| {
                    format!("{consumer} references credential {addr}, which does not resolve")
                })?;

            if spec.driver != DRIVER_NAME {
                anyhow::bail!(
                    "{consumer} lists {addr} under `credentials`, but {addr} is a `{}` target — \
                     `credentials` takes addresses of `{DRIVER_NAME}` targets. Did you mean to \
                     put it in `deps`?",
                    spec.driver
                );
            }

            let def = parse_declaration(&spec)
                .with_context(|| format!("{consumer} references credential {addr}"))?;
            resolved.push(ResolvedCredential { addr, def });
        }

        check_env_collisions(consumer, &resolved)?;
        Ok(resolved)
    }

    /// Acquire a credential's material, or return the material already held.
    ///
    /// Single-flighted per address across the process, expiry-aware, and backed by
    /// the disk tier. `parents` carries the store keys of the credentials that
    /// *led* here, so a derived credential minted under one root identity is never
    /// served to a run holding another; it is also the recursion guard.
    #[async_recursion::async_recursion]
    pub async fn acquire_credential<'a>(
        self: &'a Arc<Self>,
        rs: &'a Arc<RequestState>,
        addr: &'a Addr,
        chain: &'a Chain,
    ) -> anyhow::Result<Acquired> {
        // Before anything takes a lock. A credential that names itself would
        // otherwise park on its own cell forever — a one-line BUILD typo turning
        // into a wedged build with no diagnostic, no timeout and no error.
        chain.enter(addr)?;
        let spec = Arc::clone(self).get_spec(rs.clone(), addr).await?;
        if spec.driver != DRIVER_NAME {
            anyhow::bail!("{addr} is a `{}` target, not a credential", spec.driver);
        }
        let def = parse_declaration(&spec).with_context(|| format!("credential {addr}"))?;
        self.acquire_declared(rs, addr, &def, chain).await
    }

    async fn acquire_declared(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        def: &CredentialDef,
        chain: &Chain,
    ) -> anyhow::Result<Acquired> {
        let addr_s = addr.format();
        let cell = self.credential_cache.cell(&addr_s, chain.keys());
        let mut held = cell.lock().await;
        let now = SystemTime::now();

        if let Some(a) = held.as_ref()
            && a.material.usable_at(now, a.acquired_at)
        {
            return Ok(a.clone());
        }

        let acquired = self
            .walk_and_acquire(rs, addr, def, chain)
            .await
            .with_context(|| format!("acquire credential {addr}"))?;
        *held = Some(acquired.clone());
        Ok(acquired)
    }

    /// Walk the chain and acquire from the first applicable source.
    ///
    /// > A probe answers "is this source applicable here?", not "will it
    /// > succeed?". The first applicable source *is* the source; an acquire
    /// > failure is terminal, not a fallthrough.
    ///
    /// If a chain fell through on failure, a misconfigured role in CI would
    /// silently fall back to whatever ambient identity the runner happened to
    /// have — which is precisely the class of accident this feature prevents.
    async fn walk_and_acquire(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        def: &CredentialDef,
        chain: &Chain,
    ) -> anyhow::Result<Acquired> {
        let walk = self.walk_chain(rs, addr, def).await;
        let Some(step) = walk.iter().find(|s| s.skipped.is_none()) else {
            // A chain that applies nowhere but *could* be repaired by signing in
            // is a different failure from one that could not, and the difference
            // is the whole value of the message: one is a command to run, the
            // other is a configuration bug. The login argvs travel as structured
            // data so `--json` consumers and the terminal render the same
            // instruction.
            let repairable: Vec<Vec<String>> = walk
                .iter()
                .filter_map(|s| def.sources.get(s.index))
                .flat_map(source_login)
                .cloned()
                .collect();
            if !repairable.is_empty() {
                return Err(AuthRequiredError {
                    addr: addr.clone(),
                    source: walk.first().map(|s| s.label.clone()).unwrap_or_default(),
                    login: repairable,
                }
                .context(NoApplicableSourceError {
                    addr: addr.clone(),
                    walk,
                }));
            }
            return Err(NoApplicableSourceError {
                addr: addr.clone(),
                walk,
            }
            .into());
        };
        let index = step.index;
        let source = def
            .sources
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("chain walk named a source that is not in the chain"))?;

        // The parents this source itself needs, acquired first — their keys are
        // what makes this credential's key identity-specific. `descend` is what
        // arms the recursion guard for everything below.
        let mut inner = chain.descend(addr);
        for cred in &source.credentials {
            let cred_addr = self.parse_credential_addr(cred, addr)?;
            let got = self
                .acquire_credential(rs, &cred_addr, &inner)
                .await
                .with_context(|| format!("{addr} source {} needs {cred_addr}", step.label))?;
            inner = inner.with_key(got.key);
        }

        let key = crate::engine::credential_store::resolution_key(
            &addr.format(),
            &source_config_key(source),
            inner.keys(),
        );

        let store = self.credential_store();
        let now = SystemTime::now();
        // The entry's own acquisition time rides back with it. Substituting `now`
        // would shrink the skew margin — which is `min(60s, lifetime/2)` — for
        // exactly the entries that have been sitting longest, so a one-hour token
        // read with seventy seconds left would be handed out with a 35-second
        // margin instead of the sixty it was written to have.
        if let Some(hit) = store.get(&key, now) {
            return Ok(Acquired {
                material: hit.material,
                source: hit.source,
                source_index: index,
                acquired_at: hit.acquired_at,
                key,
                from_disk: true,
            });
        }

        // Cross-process lock, so two concurrent `heph` invocations do not both
        // drive an interactive vendor CLI for the same credential.
        let _guard = self.credential_lock_guard(&key).await?;
        // Re-check under the lock: the process we waited for may have just
        // written the entry.
        if let Some(hit) = store.get(&key, SystemTime::now()) {
            return Ok(Acquired {
                material: hit.material,
                source: hit.source,
                source_index: index,
                acquired_at: hit.acquired_at,
                key,
                from_disk: true,
            });
        }

        let acquired_at = SystemTime::now();
        let mut material = self
            .acquire_from(rs, addr, source, &key, &inner)
            .await
            .with_context(|| format!("credential {addr} via {}", step.label))?;

        // A declared `ttl` fills in for material whose source reports no expiry —
        // and is also what makes such material cacheable at all.
        if material.expires_at.is_none()
            && let Some(ttl) = def.ttl
        {
            material.expires_at = Some(
                acquired_at
                    .checked_add(ttl)
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
            );
        }

        // The no-expiry rule covers *files* too, and it cannot be applied inside
        // `put`: a producer's outputs are written before anything knows whether
        // the material has an expiry. So they are staged under a per-process
        // directory — swept of dead pids by the next `heph` — and promoted into
        // their durable home only once the material has earned the right to
        // outlive this process. An unbounded producer credential still works for
        // the whole run; what it does not do is leave a `0600` secret behind
        // that only `heph auth logout` would ever collect.
        let staged = !material.files.is_empty()
            && material
                .files
                .values()
                .any(|p| p.starts_with(store.staged_files_dir(&key)));
        if material.expires_at.is_some() && staged {
            let durable = store
                .promote_staged(&key)
                .with_context(|| format!("keep credential {addr}'s files"))?;
            for path in material.files.values_mut() {
                if let Some(name) = path.file_name() {
                    *path = durable.join(name);
                }
            }
        }
        store
            .put(&key, &addr.format(), &step.label, &material, acquired_at)
            .with_context(|| format!("cache credential {addr}"))?;

        Ok(Acquired {
            material,
            source: step.label.clone(),
            source_index: index,
            acquired_at,
            key,
            from_disk: false,
        })
    }

    /// Re-acquire from **one named source**, for a helper callback whose pinned
    /// material has lapsed.
    ///
    /// Deliberately not a chain walk. The host already chose; a callback running
    /// in a sandbox must not be able to choose differently, so this either
    /// acquires from that source or fails saying why it cannot — never silently
    /// from another. It is also why the failure message names the environment: a
    /// refresh inside a sandbox has no `HOME`, no vendor session and no OIDC
    /// endpoint, so an `exec`-sourced credential genuinely cannot refresh there,
    /// and saying so beats a vendor SDK's own error.
    pub async fn refresh_credential_source(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        source_index: usize,
    ) -> anyhow::Result<Material> {
        let spec = Arc::clone(self).get_spec(rs.clone(), addr).await?;
        let def = parse_declaration(&spec).with_context(|| format!("credential {addr}"))?;
        let source = def.sources.get(source_index).ok_or_else(|| {
            anyhow::anyhow!(
                "credential {addr} no longer has a source at position {source_index} — the \
                 declaration changed while a target was running"
            )
        })?;
        if let Some(reason) = self
            .probe_source(rs, addr, source)
            .await
            .unwrap_or_else(|e| Some(format!("{e:#}")))
        {
            anyhow::bail!(
                "credential {addr} expired and its source {} cannot be re-run here: {reason}. A \
                 credential helper is called back from inside the target's sandbox, which has no \
                 HOME, no vendor session and no CI OIDC endpoint — so a source that needs any of \
                 those can refresh only between builds, not during one. Shorten the target, or \
                 declare a source that can",
                source.label()
            );
        }
        let key = crate::engine::credential_store::resolution_key(
            &addr.format(),
            &source_config_key(source),
            &[],
        );
        self.acquire_from(rs, addr, source, &key, &Chain::root())
            .await
            .with_context(|| format!("refresh credential {addr} via {}", source.label()))
    }

    /// Run one sign-in command, in the environment its source runs in.
    ///
    /// Through `hexecrunner`, not `std::process::Command`, and for the same reason
    /// the *probe* goes through it: if a source names a runner because its tool
    /// lives in a devenv shell, then the login for that tool lives there too. A
    /// probe that correctly asks the runner, followed by a login that runs on the
    /// bare host, is two halves of one feature disagreeing about where the tool
    /// is — and the user is left being told to run a command that cannot work.
    ///
    /// Stdio is **inherited**, deliberately. A browser flow needs the terminal
    /// when there is one, and an agent needs to *see* the URL and code on its own
    /// output when there is not; capturing would strand the second case with no
    /// path forward.
    pub async fn run_login(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        source: &SourceDecl,
        argv: &[String],
    ) -> anyhow::Result<std::process::ExitStatus> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty login command"))?;
        let runner_addr = match &source.kind {
            SourceKind::Exec { runner, .. } => runner
                .as_deref()
                .map(|r| self.parse_credential_addr(r, addr))
                .transpose()?,
            _ => None,
        };
        let program_path = match runner_addr {
            None => which::which(program).unwrap_or_else(|_e| PathBuf::from(program)),
            Some(_) => PathBuf::from(program),
        };
        let spec = hproc::proc_exec::Spec {
            program: program_path,
            args: args.iter().map(std::ffi::OsString::from).collect(),
            // A login is interactive and belongs to the user, not to a build, so
            // it gets the user's own environment rather than the acquire
            // allowlist: a browser flow reads `BROWSER`, `DISPLAY`, proxy
            // settings and whatever else the vendor CLI needs to open a page.
            env: std::env::vars_os().collect(),
            cwd: self.cfg.root.clone(),
            stdin: hproc::proc_exec::StdioSpec::Inherit,
            stdout: hproc::proc_exec::StdioSpec::Inherit,
            stderr: hproc::proc_exec::StdioSpec::Inherit,
            setsid: false,
            ctty: false,
        };
        let runner_ref = match &runner_addr {
            Some(a) => hexecrunner::RunnerRef::target(rs.request_id(), a),
            None => hexecrunner::RunnerRef::local(),
        };
        let handle = hexecrunner::spawn(runner_ref, spec, rs.ctoken())
            .await
            .with_context(|| format!("run {program}"))?;
        // `spawn_wait` rather than a bare await: waiting parks its worker for the
        // child's whole lifetime, which for a browser sign-in is however long the
        // human takes.
        handle
            .spawn_wait(Arc::new(hcore::hasync::StdCancellationToken::new()))
            .await
            .context("the sign-in task panicked")?
            .with_context(|| format!("wait for {program}"))
    }

    /// Probe every source in order, recording why each was skipped.
    ///
    /// The whole walk is computed rather than stopping at the winner, because
    /// `heph auth explain` and the "no source applies" error print the same
    /// structure — so the diagnostic and the failure can never drift apart.
    pub async fn walk_chain(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        def: &CredentialDef,
    ) -> Vec<WalkStep> {
        let mut out = Vec::with_capacity(def.sources.len());
        let mut chosen = false;
        for (index, source) in def.sources.iter().enumerate() {
            let hint = source.hint.clone().unwrap_or_else(|| source.default_hint());
            if chosen {
                out.push(WalkStep {
                    index,
                    label: source.label(),
                    skipped: Some("not reached: an earlier source applied".to_string()),
                    hint,
                });
                continue;
            }
            let skipped = match self.probe_source(rs, addr, source).await {
                Ok(None) => None,
                Ok(Some(reason)) => Some(reason),
                // A probe that *errors* is a skip with the error as the reason:
                // it answers "not applicable here" just as well as a clean miss,
                // and turning it into a build failure would make an unrelated
                // broken tool on PATH fatal to a chain that has a working
                // alternative below it.
                Err(e) => Some(format!("{e:#}")),
            };
            chosen |= skipped.is_none();
            out.push(WalkStep {
                index,
                label: source.label(),
                skipped,
                hint,
            });
        }
        out
    }

    /// `Ok(None)` = applicable. `Ok(Some(reason))` = skipped, with the reason.
    async fn probe_source(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        source: &SourceDecl,
    ) -> anyhow::Result<Option<String>> {
        if let Some(when) = &source.when
            && let Some(reason) = when_unmet(when)
        {
            return Ok(Some(reason));
        }
        Ok(match &source.kind {
            SourceKind::Env { names } => names
                .iter()
                .find(|n| host_env(n).is_none())
                .map(|n| format!("{n} unset")),
            SourceKind::File { path, .. } => {
                let p = expand_home(path);
                (!p.exists()).then(|| format!("{} does not exist", p.display()))
            }
            SourceKind::Exec { run, runner, .. } => {
                let program = run.first().map(String::as_str).unwrap_or_default();
                self.probe_program(rs, addr, program, runner.as_deref())
                    .await?
            }
            SourceKind::Passthrough { paths, .. } => paths
                .values()
                .map(|p| expand_home(p))
                .find(|p| !p.exists())
                .map(|p| format!("{} does not exist", p.display())),
            SourceKind::Oidc { provider, .. } => oidc_unavailable(provider),
            // A target has no cheap probe — you cannot ask "is this applicable
            // here?" of a target without running it — so it is selected by `when`
            // alone, which is exactly right for the case it serves: "am I in CI"
            // is a `when`, "is the AWS CLI installed" is a probe.
            SourceKind::Target { .. } => None,
        })
    }

    /// Whether `program` resolves where this source would run it.
    ///
    /// Under a runner the question is about the *runner's* environment, not this
    /// host's: probing the host for a program that lives in a devenv shell answers
    /// the wrong question, and answering it wrongly is how a chain silently picks
    /// the next source down.
    async fn probe_program(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        program: &str,
        runner: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let Some(runner) = runner else {
            return Ok(which::which(program)
                .is_err()
                .then(|| format!("`{program}` not found on PATH")));
        };
        let runner_addr = self.parse_credential_addr(runner, addr)?;
        // `command -v` through a POSIX shell is the portable "does this resolve
        // here" question, and it is the only one that can be asked *inside*
        // another environment without running the tool itself.
        let spec = hproc::proc_exec::Spec {
            program: PathBuf::from("sh"),
            args: vec![
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("command -v -- \"$1\" >/dev/null"),
                std::ffi::OsString::from("sh"),
                std::ffi::OsString::from(program),
            ],
            env: vec![],
            cwd: self.cfg.root.clone(),
            stdin: hproc::proc_exec::StdioSpec::Null,
            stdout: hproc::proc_exec::StdioSpec::Piped,
            stderr: hproc::proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };
        let out = hexecrunner::output(
            hexecrunner::RunnerRef::target(rs.request_id(), &runner_addr),
            spec,
            rs.ctoken(),
        )
        .await
        .with_context(|| format!("probe `{program}` under runner {runner_addr}"))?;
        Ok((!out.status.success()).then(|| format!("`{program}` not found in {runner_addr}")))
    }

    async fn acquire_from(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        source: &SourceDecl,
        key: &str,
        chain: &Chain,
    ) -> anyhow::Result<Material> {
        match &source.kind {
            SourceKind::Env { names } => {
                let mut fields = BTreeMap::new();
                for n in names {
                    let v = host_env(n).ok_or_else(|| {
                        anyhow::anyhow!("{n} is unset — the probe said it was set")
                    })?;
                    fields.insert(env_field_name(n), v);
                }
                Ok(Material {
                    fields,
                    ..Material::default()
                })
            }
            SourceKind::File {
                path,
                fields,
                expires,
            } => {
                let p = expand_home(path);
                let body =
                    std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
                material_from_text(&body, fields, expires.as_deref())
            }
            SourceKind::Exec {
                run,
                fields,
                expires,
                runner,
                ..
            } => {
                let stdout = self
                    .run_acquire_command(rs, addr, source, run, runner.as_deref(), chain)
                    .await?;
                material_from_text(&stdout, fields, expires.as_deref())
            }
            SourceKind::Passthrough { paths, .. } => {
                let mut files = BTreeMap::new();
                for (name, path) in paths {
                    let p = expand_home(path);
                    if !p.exists() {
                        anyhow::bail!("{} does not exist", p.display());
                    }
                    files.insert(name.clone(), p);
                }
                Ok(Material {
                    files,
                    ..Material::default()
                })
            }
            SourceKind::Oidc { provider, audience } => {
                let token = self.mint_oidc_token(provider, audience).await?;
                Ok(Material {
                    fields: BTreeMap::from([("id_token".to_string(), token)]),
                    ..Material::default()
                })
            }
            SourceKind::Target { addr: target } => {
                let target_addr = self.parse_credential_addr(target, addr)?;
                self.material_from_target(rs, &target_addr, key, chain)
                    .await
            }
        }
    }

    /// Resolve an address written in a credential declaration.
    ///
    /// Relative to the *declaring credential's* package, not the consumer's:
    /// a chain is written once and read from everywhere, so what `:vault` means
    /// must not depend on who asked.
    fn parse_credential_addr(&self, raw: &str, from: &Addr) -> anyhow::Result<Addr> {
        hmodel::htaddr::parse_addr_with_base(raw, &from.package)
            .with_context(|| format!("credential {from} names {raw:?}"))
    }

    /// A source that is a target: either another credential (a delegation) or a
    /// producer whose outputs are the material.
    ///
    /// The driver of the referenced target decides which, so this needs no syntax
    /// of its own.
    async fn material_from_target(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        target: &Addr,
        key: &str,
        chain: &Chain,
    ) -> anyhow::Result<Material> {
        let spec = Arc::clone(self).get_spec(rs.clone(), target).await?;
        if spec.driver == DRIVER_NAME {
            // Delegation: take that credential's material, apply this
            // credential's presentation. `chain` already includes the delegating
            // credential, so `a → a` and `a → b → a` are caught rather than
            // parking on a held cell.
            return Ok(self.acquire_credential(rs, target, chain).await?.material);
        }
        self.material_from_producer(rs, target, key).await
    }

    /// Run a producer target and take its outputs into the credential store.
    ///
    /// **This is the interception the design prices as the one piece of real
    /// engine work.** A target's outputs normally land in the artifact store, and
    /// material in the build cache is the invariant this whole feature exists to
    /// preserve. So a producer is run through `execute` directly and its output
    /// bytes are copied into the credential store at `0600`; `cache_locally` is
    /// never reached and no artifact for it ever exists.
    ///
    /// `cache = False` is required and enforced here rather than documented,
    /// because a cacheable producer would be pushed to the shared remote before
    /// anything here could intervene.
    async fn material_from_producer(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        target: &Addr,
        key: &str,
    ) -> anyhow::Result<Material> {
        let spec = Arc::clone(self).get_spec(rs.clone(), target).await?;
        let def = Arc::clone(self).get_def(rs.clone(), target).await?;
        if def.target_def.cache.enabled {
            anyhow::bail!(
                "{target} is used as a credential source, so it must set `cache = False` — its \
                 outputs are material, and a cacheable target's outputs are written to the local \
                 cache and pushed to the shared remote automatically"
            );
        }
        let linked = Arc::clone(self)
            .link(rs.clone(), Arc::clone(&def.target_def))
            .await
            .with_context(|| format!("link credential source {target}"))?;
        let meta = Arc::clone(self).meta(rs.clone(), target).await?;
        self.gate_approval(rs, &spec, &linked)
            .await
            .with_context(|| format!("approval {target}"))?;

        // The per-addr execute lock, taken by hand because this path deliberately
        // sidesteps `result_addr`. Without it two runs of one producer — this one
        // and an ordinary `heph run` of the same target — both claim
        // `<home>/sandbox/<pkg>/__target_<name>`, and the second claim's
        // `remove_stale` deletes the first's live tree. `credential_lock` does not
        // cover this: it is keyed on a resolution key, not on an address.
        let _w = self
            .acquire_with_notice(rs, target, self.result_lock().write(target, rs.ctoken()))
            .await?;

        let run = Arc::clone(self)
            .execute(
                rs.clone(),
                target,
                &spec,
                &linked,
                &meta.hashin,
                None,
                false,
                false,
                // The outputs are material: a failure must not leave them in a
                // sandbox kept for diagnostics.
                true,
            )
            .await;

        let (artifacts, teardown, guards) = match run {
            Ok(v) => v,
            Err(e) => {
                return Err(e).with_context(|| format!("run credential source {target}"));
            }
        };

        let material = read_producer_outputs(&artifacts, self.credential_store(), key)
            .with_context(|| format!("read credential material from {target}"));

        drop(guards);
        teardown.complete(target.format());
        material
    }

    /// Run an inline `exec` source's command and return its stdout.
    ///
    /// Goes through `hexecrunner` like every other subprocess heph spawns, so a
    /// source can name a runner and its command runs inside that environment —
    /// and so a source that shells out to a secret manager can be handed the
    /// credentials it needs, presented exactly as they would be to a consumer.
    async fn run_acquire_command(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        source: &SourceDecl,
        run: &[String],
        runner: Option<&str>,
        chain: &Chain,
    ) -> anyhow::Result<String> {
        let (program, args) = run
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty credential command"))?;

        // The source's own credentials, presented to its subprocess. The machinery
        // is the consumer's, unchanged: an acquire subprocess is just another
        // consumer of a presentation.
        //
        // The presented files ride an RAII guard rather than a deferred cleanup
        // list: this function can leave by `?` (a second credential's
        // presentation fails) or by being dropped (cancellation), and both used to
        // leave a 0600 token on disk with nothing to collect it.
        let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = acquire_base_env();
        let mut presented = CredentialTeardown::none();
        for cred in &source.credentials {
            let cred_addr = self.parse_credential_addr(cred, addr)?;
            let cred_spec = Arc::clone(self).get_spec(rs.clone(), &cred_addr).await?;
            let cred_def = parse_declaration(&cred_spec)?;
            let acquired = self.acquire_credential(rs, &cred_addr, chain).await?;
            let source_decl = cred_def.sources.get(acquired.source_index);
            let present = match source_decl {
                Some(sd) => cred_def.presentation_for(sd)?,
                None => anyhow::bail!("credential {cred_addr} chose a source that vanished"),
            };
            // Unique per acquisition, not per resolution key: two credentials
            // naming one source-credential resolve to the same key, and a shared
            // directory means one acquisition's teardown deletes the other's
            // files while its subprocess is still reading them.
            let dir = presented.add_under(self.credential_store().root())?;
            let mount = self
                .present_material(&cred_addr, present, &acquired, &dir)
                .with_context(|| format!("present {cred_addr} to a credential command"))?;
            env.extend(
                mount
                    .env
                    .into_iter()
                    .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v))),
            );
        }

        let runner_addr = runner
            .map(|r| self.parse_credential_addr(r, addr))
            .transpose()?;
        // The probe answered with `which::which` against *this* PATH, so the
        // spawn has to ask the same question or the two disagree: a bare program
        // name spawned into a cleared environment resolves against `confstr(_CS_PATH)`,
        // which is `/usr/bin:/bin` — where neither `aws` nor `gcloud` nor `vault`
        // is installed on any supported target. Under a runner the runner supplies
        // the PATH and resolves the name itself, so leave it alone there.
        let program_path = match runner_addr {
            None => which::which(program).unwrap_or_else(|_e| PathBuf::from(program)),
            Some(_) => PathBuf::from(program),
        };
        let spec = hproc::proc_exec::Spec {
            program: program_path,
            args: args.iter().map(std::ffi::OsString::from).collect(),
            env,
            cwd: self.cfg.root.clone(),
            stdin: hproc::proc_exec::StdioSpec::Null,
            stdout: hproc::proc_exec::StdioSpec::Piped,
            stderr: hproc::proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };
        let runner_ref = match &runner_addr {
            Some(a) => hexecrunner::RunnerRef::target(rs.request_id(), a),
            None => hexecrunner::RunnerRef::local(),
        };
        let out = hexecrunner::output(runner_ref, spec, rs.ctoken())
            .await
            .with_context(|| format!("run {program}"))?;
        drop(presented);
        if !out.status.success() {
            // stderr, never stdout: stdout is the credential.
            anyhow::bail!(
                "{program} exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        String::from_utf8(out.stdout)
            .with_context(|| format!("{program} printed something that is not UTF-8"))
    }

    /// Mint a workload-identity token from the CI provider's own endpoint.
    async fn mint_oidc_token(
        self: &Arc<Self>,
        provider: &str,
        audience: &str,
    ) -> anyhow::Result<String> {
        match provider {
            "github_actions" => {
                let url = host_env(GHA_OIDC_URL).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{GHA_OIDC_URL} is unset — add `permissions: {{ id-token: write }}` to the \
                         job"
                    )
                })?;
                let token = host_env(GHA_OIDC_TOKEN).ok_or_else(|| {
                    anyhow::anyhow!("{GHA_OIDC_TOKEN} is unset — the OIDC endpoint needs it")
                })?;
                let url = if audience.is_empty() {
                    url
                } else {
                    format!(
                        "{url}{}audience={}",
                        if url.contains('?') { "&" } else { "?" },
                        urlencode(audience)
                    )
                };
                let body: String = hcore::blocking::run(move || {
                    let resp = reqwest::blocking::Client::new()
                        .get(&url)
                        .bearer_auth(&token)
                        .send()
                        .context("call the OIDC endpoint")?;
                    let status = resp.status();
                    let text = resp.text().context("read the OIDC response")?;
                    if !status.is_success() {
                        anyhow::bail!("the OIDC endpoint answered {status}");
                    }
                    anyhow::Ok(text)
                })
                .await?;
                let doc: serde_json::Value = serde_json::from_str(&body)
                    .context("the OIDC endpoint did not answer with JSON")?;
                doc.get("value")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("the OIDC response carried no `value`"))
            }
            // A token some other CI provider already put where heph can find it.
            // Deliberately generic: the alternative is a provider list with no end.
            "generic" => {
                if let Some(t) = host_env(GENERIC_OIDC_TOKEN) {
                    return Ok(t);
                }
                let path = host_env(GENERIC_OIDC_TOKEN_FILE).ok_or_else(|| {
                    anyhow::anyhow!(
                        "neither {GENERIC_OIDC_TOKEN} nor {GENERIC_OIDC_TOKEN_FILE} is set"
                    )
                })?;
                std::fs::read_to_string(&path)
                    .map(|s| s.trim().to_string())
                    .with_context(|| format!("read {path}"))
            }
            other => anyhow::bail!("unknown oidc provider {other:?}"),
        }
    }

    /// Acquire and present every credential a target declared.
    ///
    /// Returns the mounts the driver applies, plus a teardown that deletes the
    /// presented files. Runs strictly *downstream* of runner preparation, which is
    /// the rule that keeps the devenv `wrap` runner's environment capture — a
    /// cached, remotely-shippable `runner.json` — from ever seeing material.
    pub(crate) async fn acquire_and_present(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        consumer: &Addr,
        sandbox_dir: &Path,
        resolved: &[ResolvedCredential],
    ) -> anyhow::Result<(Vec<CredentialMount>, CredentialTeardown)> {
        if resolved.is_empty() {
            return Ok((Vec::new(), CredentialTeardown::none()));
        }
        let root = credential_dir(sandbox_dir);
        let mut mounts = Vec::with_capacity(resolved.len());
        for rc in resolved {
            let acquired = self
                .acquire_declared(rs, &rc.addr, &rc.def, &Chain::root())
                .await
                .with_context(|| format!("{consumer} needs credential {}", rc.addr))?;
            let source = rc.def.sources.get(acquired.source_index).ok_or_else(|| {
                anyhow::anyhow!("credential {} chose a source that vanished", rc.addr)
            })?;
            let present = rc.def.presentation_for(source)?;
            let dir = root.join(sanitize_addr(&rc.addr));
            let mount = self
                .present_material(&rc.addr, present, &acquired, &dir)
                .with_context(|| format!("present credential {} to {consumer}", rc.addr))?;
            mounts.push(mount);
        }
        Ok((mounts, CredentialTeardown::at(root)))
    }

    /// Turn material plus a presentation into a mount, writing any files.
    pub(crate) fn present_material(
        self: &Arc<Self>,
        addr: &Addr,
        present: &Presentation,
        acquired: &Acquired,
        dir: &Path,
    ) -> anyhow::Result<CredentialMount> {
        let material = &acquired.material;
        let helper_command = helper_command()?;

        // Written before anything that might name it, because every helper
        // document's argv points at it. Only for a helper presentation: nothing
        // else calls back, so nothing else needs the material on disk.
        let pin_path = dir.join(HELPER_PIN_FILE);
        if present.helper.is_some() {
            let pin = HelperPin {
                addr: addr.format(),
                root: self.cfg.root.clone(),
                source_index: acquired.source_index,
                acquired_at: acquired
                    .acquired_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                material: material.clone(),
            };
            crate::engine::credential_store::write_private_file(
                &pin_path,
                &serde_json::to_vec(&pin).context("serialize the credential helper pin")?,
            )
            .with_context(|| format!("write the credential helper pin for {addr}"))?;
        }
        let helper_args = |dialect: Dialect| {
            vec![
                crate::engine::credential::AUTH_HELPER_SUBCOMMAND.to_string(),
                dialect.as_str().to_string(),
                "--pin".to_string(),
                pin_path.to_string_lossy().into_owned(),
            ]
        };

        // Files first: an `env` template may point at one, and a helper document
        // may be one.
        let mut files: BTreeMap<String, PathBuf> = material.files.clone();
        let mut path_prefix: Vec<PathBuf> = Vec::new();
        let mut env: BTreeMap<String, String> = BTreeMap::new();

        // A file's *content* may name another presented file — `heph.auth.gcp`'s
        // `external_account` document points at `${file:token}` — so one pass in
        // map order is not enough. Retry until a round makes no progress, then
        // report the error from the file that is still stuck.
        //
        // Progress-bounded rather than depth-bounded: it terminates because every
        // round either resolves at least one file or stops, and it does not
        // silently cap how deep a legitimate chain may be. Refusing to iterate
        // further than that is what keeps this from becoming a template language.
        let dialect = present.helper.as_ref().map(|h| h.dialect()).transpose()?;
        let mut pending: Vec<(&String, &String)> = present.files.iter().collect();
        let mut stuck: Option<(String, anyhow::Error)> = None;
        while !pending.is_empty() {
            let before = pending.len();
            let mut deferred = Vec::with_capacity(before);
            stuck = None;
            for (name, tmpl) in pending {
                match render(
                    tmpl,
                    material,
                    &files,
                    dialect,
                    &helper_command,
                    &helper_args,
                ) {
                    Ok(body) => {
                        let path = dir.join(name);
                        crate::engine::credential_store::write_private_file(&path, body.as_bytes())
                            .with_context(|| format!("write presented file `{name}`"))?;
                        files.insert(name.clone(), path);
                    }
                    Err(e) => {
                        // Keep the first round's real error — "no material field
                        // `tokn`" is the message the author needs, not a generic
                        // "does not resolve".
                        if stuck.is_none() {
                            stuck = Some((name.clone(), e));
                        }
                        deferred.push((name, tmpl));
                    }
                }
            }
            // No file resolved this round, so none ever will.
            if deferred.len() == before {
                break;
            }
            pending = deferred;
        }
        if let Some((name, err)) = stuck {
            return Err(err.context(format!("credential {addr}: presented file `{name}`")));
        }

        // Helper documents, which are files heph writes rather than the author.
        if let Some(helper) = &present.helper {
            let dialect = helper.dialect()?;
            match dialect {
                Dialect::Aws => {
                    let body = format!(
                        "[default]\ncredential_process = {}\n",
                        shell_join(&helper_command, &helper_args(dialect))
                    );
                    let path = dir.join("aws-config");
                    crate::engine::credential_store::write_private_file(&path, body.as_bytes())?;
                    env.insert(
                        "AWS_CONFIG_FILE".to_string(),
                        path.to_string_lossy().into_owned(),
                    );
                    env.insert("AWS_PROFILE".to_string(), "default".to_string());
                }
                Dialect::Gcp => {
                    let audience = helper.audience.clone().unwrap_or_default();
                    let mut doc = serde_json::json!({
                        "type": "external_account",
                        "audience": audience,
                        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
                        "token_url": "https://sts.googleapis.com/v1/token",
                        "credential_source": {
                            "executable": {
                                "command": shell_join(&helper_command, &helper_args(dialect)),
                                "timeout_millis": 30_000,
                            }
                        },
                    });
                    if let Some(sa) = &helper.impersonate
                        && let Some(obj) = doc.as_object_mut()
                    {
                        obj.insert(
                            "service_account_impersonation_url".to_string(),
                            serde_json::Value::String(format!(
                                "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{sa}:generateAccessToken"
                            )),
                        );
                    }
                    let path = dir.join("gcp-adc.json");
                    crate::engine::credential_store::write_private_file(
                        &path,
                        doc.to_string().as_bytes(),
                    )?;
                    let p = path.to_string_lossy().into_owned();
                    env.insert("GOOGLE_APPLICATION_CREDENTIALS".to_string(), p.clone());
                    env.insert("CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE".to_string(), p);
                    // No Google SDK will run an executable credential source
                    // unless this is set, and forgetting it by hand produces an
                    // error from inside the SDK rather than from heph.
                    env.insert(
                        "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES".to_string(),
                        "1".to_string(),
                    );
                }
                Dialect::Docker => {
                    let cred_helpers: serde_json::Map<String, serde_json::Value> = helper
                        .registries
                        .iter()
                        .map(|r| (r.clone(), serde_json::Value::String("heph".to_string())))
                        .collect();
                    let doc = serde_json::json!({ "credHelpers": cred_helpers });
                    let cfg_dir = dir.join("docker");
                    crate::engine::credential_store::write_private_file(
                        &cfg_dir.join("config.json"),
                        doc.to_string().as_bytes(),
                    )?;
                    // Docker resolves a helper by executable name, so the shim
                    // has to exist under that exact name somewhere on PATH.
                    let bin = dir.join("bin");
                    let shim = bin.join("docker-credential-heph");
                    let script = format!(
                        "#!/bin/sh\nexec {} \"$@\"\n",
                        shell_join(&helper_command, &helper_args(dialect))
                    );
                    crate::engine::credential_store::write_private_file(&shim, script.as_bytes())?;
                    make_executable(&shim)?;
                    env.insert(
                        "DOCKER_CONFIG".to_string(),
                        cfg_dir.to_string_lossy().into_owned(),
                    );
                    path_prefix.push(bin);
                }
                Dialect::Git => {
                    // Through the environment rather than a gitconfig: it avoids
                    // generating a file, avoids any chance of touching the
                    // developer's real one, and the settings vanish with the
                    // process.
                    let helper_value =
                        format!("!{}", shell_join(&helper_command, &helper_args(dialect)));
                    env.insert(
                        "GIT_CONFIG_COUNT".to_string(),
                        helper.hosts.len().to_string(),
                    );
                    for (i, host) in helper.hosts.iter().enumerate() {
                        env.insert(
                            format!("GIT_CONFIG_KEY_{i}"),
                            format!("credential.https://{host}.helper"),
                        );
                        env.insert(format!("GIT_CONFIG_VALUE_{i}"), helper_value.clone());
                    }
                }
                // The one dialect where heph writes no document: a kubeconfig is
                // not purely a credential format, so the author templates it and
                // places the callback with `${helper:command}` / `${helper:args}`.
                Dialect::Kubernetes => {}
            }
        }

        let dialect = present.helper.as_ref().map(|h| h.dialect()).transpose()?;
        for (name, tmpl) in &present.env {
            let value = render(
                tmpl,
                material,
                &files,
                dialect,
                &helper_command,
                &helper_args,
            )
            .with_context(|| format!("credential {addr}: environment variable `{name}`"))?;
            if value.len() > CREDENTIAL_ENV_MAX_BYTES {
                anyhow::bail!(
                    "credential {addr}: `{name}` is {} bytes, over the {CREDENTIAL_ENV_MAX_BYTES}-byte \
                     limit for a presented environment value. Something that large is a document, \
                     not a token — present it as a `files` entry and point a variable at the path",
                    value.len()
                );
            }
            // A helper document already claimed some names; an author's own
            // template must not silently lose to one, or win over one.
            if let Some(existing) = env.get(name)
                && existing != &value
            {
                anyhow::bail!(
                    "credential {addr}: `{name}` is set both by the `{}` helper and by this \
                     presentation's `env`. One would shadow the other — drop it from `env`, or \
                     use a different variable",
                    dialect.map(Dialect::as_str).unwrap_or("?")
                );
            }
            env.insert(name.clone(), value);
        }

        Ok(CredentialMount {
            addr: addr.clone(),
            env,
            path_prefix,
            // Every material *field*, whether or not this presentation uses it —
            // so a credential that presents only `${access_key_id}` still scrubs
            // its session token, which is the value a build step is most likely
            // to echo.
            //
            // File-backed material is deliberately **not** here. A path is not a
            // secret and redacting it would mangle ordinary output; what is in
            // the file is the file's problem, and files are `0600` inside a
            // sandbox that is deleted at run end. The stated consequence is that
            // a target which `cat`s its own kubeconfig puts that token in
            // `log.txt` — see `docs/CREDENTIALS.md`, "Redaction".
            //
            // The floor is applied here so a driver receives only what it can
            // safely scrub.
            redact: material
                .secrets()
                .filter(|s| s.len() >= hplugin::driver::REDACT_MIN_LEN)
                .map(str::to_string)
                .collect(),
        })
    }

    /// Drop every credential this process has acquired. `heph auth logout`.
    pub fn forget_credentials(&self) {
        self.credential_cache.clear();
    }

    fn credential_store(&self) -> CredentialStore {
        CredentialStore::new(&self.home)
    }

    /// The cross-process acquisition lock for one resolution key.
    ///
    /// Two concurrent `heph` invocations must not both drive an interactive
    /// vendor CLI for the same credential — that is two browser windows and two
    /// sessions where the user asked for one.
    async fn credential_lock_guard(
        self: &Arc<Self>,
        key: &str,
    ) -> anyhow::Result<Box<dyn std::any::Any + Send>> {
        self.credential_lock.write(key).await
    }
}

/// The cross-process acquisition lock, keyed by resolution key.
///
/// A [`KeyedRWLock`](hlock::hlock::KeyedRWLock) taken only for write: acquisition
/// is never a shared operation, so the read half would buy nothing. Its whole job
/// is to stop two concurrent `heph` invocations both driving an interactive vendor
/// CLI for the same credential — which is two browser windows and two sessions
/// where the user asked for one.
pub enum CredentialLock {
    /// `flock(2)` files under `<home>/lock/auth/`.
    Fs(hlock::hlock::KeyedRWLock<String, hlock::hlock::FRWLock>),
    /// In-process only. Tests, and anything that has opted out of file locking.
    Mem(hlock::hlock::KeyedRWLock<String, hlock::hlock::MemRWLock>),
}

impl CredentialLock {
    pub fn new(backend: crate::engine::result_lock::LockBackend, dir: PathBuf) -> Self {
        match backend {
            crate::engine::result_lock::LockBackend::Fs => {
                Self::Fs(hlock::hlock::KeyedRWLock::new(move |key: &String| {
                    hlock::hlock::FRWLock::new(dir.join(format!("{key}.auth.lock")))
                }))
            }
            crate::engine::result_lock::LockBackend::Mem => {
                Self::Mem(hlock::hlock::KeyedRWLock::new(|_k| {
                    hlock::hlock::MemRWLock::default()
                }))
            }
        }
    }

    /// Take the exclusive guard for `key`. Type-erased: this code only ever holds
    /// and drops it.
    pub async fn write(&self, key: &str) -> anyhow::Result<Box<dyn std::any::Any + Send>> {
        // Deliberately not cancellable-scoped: an acquisition already in flight
        // in another process is what this waits on, and abandoning the wait would
        // just start a second one.
        let ct = hcore::hasync::StdCancellationToken::new();
        Ok(match self {
            Self::Fs(l) => {
                Box::new(l.write(key.to_string(), &ct).await?) as Box<dyn std::any::Any + Send>
            }
            Self::Mem(l) => {
                Box::new(l.write(key.to_string(), &ct).await?) as Box<dyn std::any::Any + Send>
            }
        })
    }
}

/// What the host hands a helper callback, so the callback does not have to
/// rediscover it.
///
/// **This is the fix for the sharpest hole a callback presentation has.** A
/// helper runs inside the *target's* sandbox, where the environment is cleared,
/// `PATH` is the sandbox's, and there is no `HOME` — so a helper that re-walked
/// the chain would probe a different environment and could pick a *different
/// source*, silently acquiring a different identity than the one the host chose.
/// On a GitHub Actions runner that is concretely: the host wins on
/// `oidc(github_actions)`, the callback finds no OIDC endpoint in the sandbox,
/// falls to `exec(aws)`, and the build runs against whatever ambient identity the
/// runner happened to have — the exact accident the chain exists to prevent.
///
/// So the resolution is pinned. The document carries the material itself (the
/// common case answers from it with no engine, no probe and no environment
/// dependency at all) and, for the refresh case, the *index of the winning
/// source*, so a re-acquisition can only ever use the source the host chose.
///
/// It lives at `0600` beside the presented files, inside the sandbox, and is
/// deleted with them at run end.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct HelperPin {
    /// The credential, for diagnostics and for a refresh.
    pub addr: String,
    /// The workspace, because a sandbox has its own cwd.
    pub root: PathBuf,
    /// Which source the host chose. A refresh may use this one and no other.
    pub source_index: usize,
    /// Unix seconds, so `Material::usable_at` means the same thing here.
    pub acquired_at: u64,
    pub material: Material,
}

/// The file a [`HelperPin`] is written to, beside the presented files.
pub const HELPER_PIN_FILE: &str = "pin.json";

/// `heph __auth-helper` — the hidden subcommand a tool calls back into.
///
/// Parsed before the argument parser, since its argv and stdin belong to the
/// calling tool rather than to heph.
pub const AUTH_HELPER_SUBCOMMAND: &str = "__auth-helper";

const GHA_OIDC_URL: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
const GHA_OIDC_TOKEN: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";
const GENERIC_OIDC_TOKEN: &str = "HEPH_OIDC_TOKEN";
const GENERIC_OIDC_TOKEN_FILE: &str = "HEPH_OIDC_TOKEN_FILE";

/// Deletes a run's presented credential files.
///
/// Fires whatever the outcome. A failed target's sandbox is deliberately kept for
/// diagnostics — the failure paragraph reads its log tail lazily — and the log
/// tail is what makes that useful, not the token.
pub struct CredentialTeardown {
    dirs: Vec<PathBuf>,
}

impl CredentialTeardown {
    fn none() -> Self {
        Self { dirs: Vec::new() }
    }

    fn at(dir: PathBuf) -> Self {
        Self { dirs: vec![dir] }
    }

    /// Claim a fresh, uniquely-named private directory under `root`, owned by
    /// this guard.
    ///
    /// Unique per call rather than keyed on anything: two acquisitions of the
    /// same credential resolve to the same key, and a shared directory means one
    /// teardown deletes files another subprocess is still reading.
    fn add_under(&mut self, root: &Path) -> anyhow::Result<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = root.join("acquire").join(format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        crate::engine::credential_store::ensure_private_dir(&dir)?;
        self.dirs.push(dir.clone());
        Ok(dir)
    }
}

impl Drop for CredentialTeardown {
    fn drop(&mut self) {
        for dir in self.dirs.drain(..) {
            drop(std::fs::remove_dir_all(dir));
        }
    }
}

/// The host variables an acquire subprocess is handed, and nothing else.
///
/// An explicit allowlist rather than an empty environment or the whole host's.
/// Empty is what the first version did, and it does not work: `execve` with a
/// cleared environ resolves a bare program name against `confstr(_CS_PATH)`, and
/// every vendor CLI worth acquiring from (`aws`, `gcloud`, `az`, `vault`) reads
/// `$HOME` to find the session cache the probe just decided was there. Passing
/// the whole host environment is the other extreme, and is how a source quietly
/// picks up an ambient identity the declaration never mentioned — the accident
/// the chain exists to prevent.
///
/// So: enough to *find and run* a tool, and nothing that names an identity. A
/// source that needs more says so with `credentials`, which is presented on top
/// of this.
fn acquire_base_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    // `PATH` so the program can be found at all and can find its own libraries;
    // `HOME` because every vendor CLI keeps its session under it, which is what
    // the probe just decided was there; `TMPDIR` because some refuse to start
    // without one; `LANG`/`LC_ALL` so a CLI that formats its output formats it
    // the same way twice; `TERM` because one that finds none behaves better than
    // one that guesses.
    const ALLOWED: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM"];
    ALLOWED
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (std::ffi::OsString::from(*k), v)))
        .collect()
}

/// The part of a source declaration that decides which material it yields.
///
/// The **whole** declaration, not just its `kind`. Two `exec` sources running one
/// command under different `when`s serialize their kinds identically, and keying
/// on that alone would let a build under `when = "env:STAGING"` be served the
/// entry a build under `when = "env:PROD"` wrote — which is a build acting as the
/// wrong principal, the one thing this key exists to prevent.
fn source_config_key(source: &SourceDecl) -> String {
    serde_json::to_string(source).unwrap_or_default()
}

/// Where a run's presented credential files go: beside the workspace directory,
/// never inside it. See the module docs — output collection walks the workspace
/// directory and packs every regular file it finds.
pub fn credential_dir(sandbox_dir: &Path) -> PathBuf {
    sandbox_dir.join(".heph").join("auth")
}

/// One path component per credential, from its *full* address.
///
/// The address rather than the name, so two credentials called `aws` in different
/// packages cannot collide.
fn sanitize_addr(addr: &Addr) -> String {
    addr.format()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The material field name an `env` source yields for a variable: the name,
/// lowercased. `GITHUB_TOKEN` → `${github_token}`.
fn env_field_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Read a host environment variable, treating empty as unset.
///
/// Empty-as-unset because CI systems routinely export a variable with no value
/// for a secret that was not configured, and treating that as "present" makes a
/// probe pick a source that cannot possibly work.
fn host_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `~` expansion, because every vendor CLI writes under it and a declaration that
/// had to spell out `/Users/x` would not be portable between a laptop and CI.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// `Some(reason)` when this `when` does not hold here.
fn when_unmet(when: &When) -> Option<String> {
    match when {
        When::Ci => (detected_ci_provider(&|n| host_env(n)).is_none())
            .then(|| "not running in a CI provider heph recognizes".to_string()),
        When::CiProvider(p) => {
            let got = detected_ci_provider(&|n| host_env(n));
            (got != Some(p.as_str())).then(|| match got {
                Some(other) => format!("running in {other}, not {p}"),
                None => format!("not running in {p}"),
            })
        }
        When::Interactive => (!std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .then(|| "no terminal is attached".to_string()),
        When::Env(n) => host_env(n).is_none().then(|| format!("{n} unset")),
        When::Os(o) => {
            let here = if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "linux"
            };
            (here != o).then(|| format!("running on {here}, not {o}"))
        }
    }
}

/// `Some(reason)` when this OIDC provider is not present here.
fn oidc_unavailable(provider: &str) -> Option<String> {
    match provider {
        "github_actions" => host_env(GHA_OIDC_URL)
            .is_none()
            .then(|| format!("{GHA_OIDC_URL} unset")),
        "generic" => (host_env(GENERIC_OIDC_TOKEN).is_none()
            && host_env(GENERIC_OIDC_TOKEN_FILE).is_none())
        .then(|| format!("neither {GENERIC_OIDC_TOKEN} nor {GENERIC_OIDC_TOKEN_FILE} is set")),
        other => Some(format!("unknown oidc provider {other:?}")),
    }
}

/// Build material from a source's stdout or a file's contents.
///
/// With no field map the whole text is `${value}` — which is what a command
/// printing a bare token needs, and it is deliberately the *only* thing heph
/// guesses about a vendor's output.
pub(crate) fn material_from_text(
    text: &str,
    fields: &BTreeMap<String, String>,
    expires: Option<&str>,
) -> anyhow::Result<Material> {
    if fields.is_empty() && expires.is_none() {
        return Ok(Material {
            fields: BTreeMap::from([("value".to_string(), text.trim().to_string())]),
            ..Material::default()
        });
    }
    let doc: serde_json::Value = serde_json::from_str(text.trim()).context(
        "the source printed something that is not JSON, but `fields`/`expires` name JSON keys",
    )?;
    let mut out = BTreeMap::new();
    for (field, key) in fields {
        let v = json_path(&doc, key)
            .ok_or_else(|| anyhow::anyhow!("the source's JSON has no `{key}`"))?;
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => anyhow::bail!("`{key}` is {other}, which is not a credential field"),
        };
        out.insert(field.clone(), s);
    }
    let expires_at = expires
        .and_then(|key| json_path(&doc, key))
        .map(parse_expiry)
        .transpose()?;
    Ok(Material {
        fields: out,
        files: BTreeMap::new(),
        expires_at,
    })
}

/// A dotted path into a JSON document: `data.token`.
fn json_path<'a>(doc: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    key.split('.').try_fold(doc, |d, part| d.get(part))
}

/// An expiry, as vendors actually report them.
///
/// A number is a **duration in seconds** (Vault's `lease_duration`, OAuth's
/// `expires_in`); a string is an absolute RFC 3339 timestamp (AWS's `Expiration`).
/// Told apart by type, because vendors do both and a declaration should not have
/// to say which.
fn parse_expiry(v: &serde_json::Value) -> anyhow::Result<u64> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match v {
        serde_json::Value::Number(n) => {
            let secs = n
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("expiry {n} is not a whole number of seconds"))?;
            Ok(now.saturating_add(secs))
        }
        serde_json::Value::String(s) => {
            // A string that is all digits is a Unix timestamp, which some tools
            // emit; anything else must be RFC 3339.
            if let Ok(secs) = s.parse::<u64>() {
                return Ok(secs);
            }
            let ts = chrono::DateTime::parse_from_rfc3339(s)
                .with_context(|| format!("expiry {s:?} is not an RFC 3339 timestamp"))?;
            Ok(ts.timestamp().max(0).cast_unsigned())
        }
        other => anyhow::bail!("expiry {other} is neither a duration nor a timestamp"),
    }
}

/// Substitute a presentation template.
///
/// Single-pass by construction (see [`hcore::template`]): material containing
/// `${` is never re-interpreted.
fn render(
    tmpl: &str,
    material: &Material,
    files: &BTreeMap<String, PathBuf>,
    dialect: Option<Dialect>,
    helper_command: &Path,
    helper_args: &dyn Fn(Dialect) -> Vec<String>,
) -> anyhow::Result<String> {
    hcore::template::render(tmpl, |r: &Ref<'_>| match r.kind {
        None => material.fields.get(r.arg).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "no material field `{}` — this source yields {}",
                r.arg,
                render_names(material.fields.keys())
            )
        }),
        Some("file") => files
            .get(r.arg)
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no presented file `{}` — this credential presents {}",
                    r.arg,
                    render_names(files.keys())
                )
            }),
        Some("helper") => {
            let dialect = dialect.ok_or_else(|| {
                anyhow::anyhow!(
                    "`${{helper:{}}}` needs a `helper` dialect on this presentation",
                    r.arg
                )
            })?;
            match r.arg {
                "command" => Ok(helper_command.to_string_lossy().into_owned()),
                "args" => Ok(serde_json::to_string(&helper_args(dialect))
                    .unwrap_or_else(|_e| "[]".to_string())),
                other => anyhow::bail!("unknown `${{helper:{other}}}`"),
            }
        }
        Some(other) => anyhow::bail!("unknown template kind `{other}`"),
    })
}

fn render_names<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = names.map(String::as_str).collect();
    if v.is_empty() {
        "nothing".to_string()
    } else {
        v.join(", ")
    }
}

/// The absolute path of this heph binary, for a config document that calls back.
fn helper_command() -> anyhow::Result<PathBuf> {
    std::env::current_exe()
        .context("cannot determine this heph binary's path, which a credential helper has to name")
}

/// Render an argv the way a config file that is parsed by a shell expects it.
fn shell_join(program: &Path, args: &[String]) -> String {
    std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
            {
                a
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn make_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

/// Percent-encode an audience for the OIDC endpoint's query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Take a producer target's outputs into the credential store.
///
/// One convention, not configuration: an output group named `credential` is read
/// as JSON and its top-level keys become fields; every other group becomes a named
/// file. An inline `exec` source needs a `fields` map instead, and the asymmetry is
/// deliberate — a target's `run` can pipe through `jq` and shape its own output,
/// so it does not need one; a vendor's stdout is whatever the vendor decided, so
/// it does.
fn read_producer_outputs(
    artifacts: &[crate::engine::driver::outputartifact::OutputArtifact],
    store: CredentialStore,
    key: &str,
) -> anyhow::Result<Material> {
    use crate::engine::driver::outputartifact::Type;
    use hcore::hartifactcontent::{Content as _, WalkEntryKind};

    // Staged, not durable: whether these bytes may outlive this process is not
    // known until the material's expiry is, which is only after every group has
    // been read.
    store.sweep_dead_staged();
    let dir = store.staged_files_dir(key);
    drop(std::fs::remove_dir_all(&dir));
    let mut fields = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut expires_at = None;

    for artifact in artifacts
        .iter()
        .filter(|a| matches!(a.r#type, Type::Output))
    {
        let mut body = Vec::new();
        for entry in artifact.walk()? {
            let entry = entry?;
            if let WalkEntryKind::File { mut data, .. } = entry.kind {
                std::io::copy(&mut data, &mut body)
                    .with_context(|| format!("read credential output `{}`", artifact.group))?;
            }
        }
        // Same escape as a presented file name, and the same fix: the group name
        // becomes one path component under the credential store.
        hbuiltins::plugincredential::present::validate_file_name(&artifact.group)
            .with_context(|| format!("credential source output group `{}`", artifact.group))?;
        if artifact.group == CREDENTIAL_OUTPUT_GROUP {
            let text = String::from_utf8(body).context(
                "the `credential` output group is read as JSON, and this is not valid UTF-8",
            )?;
            let m = material_from_json_object(&text)?;
            fields.extend(m.fields);
            expires_at = expires_at.or(m.expires_at);
        } else {
            let path = dir.join(&artifact.group);
            crate::engine::credential_store::write_private_file(&path, &body)?;
            files.insert(artifact.group.clone(), path);
        }
    }

    if fields.is_empty() && files.is_empty() {
        anyhow::bail!(
            "the target produced no outputs, so there is no material — a credential source names \
             its material in `out`, with the group `{CREDENTIAL_OUTPUT_GROUP}` read as JSON"
        );
    }
    Ok(Material {
        fields,
        files,
        expires_at,
    })
}

/// The reserved output-group name whose contents are read as JSON fields.
pub const CREDENTIAL_OUTPUT_GROUP: &str = "credential";

/// Every top-level key of a JSON object becomes a field.
fn material_from_json_object(text: &str) -> anyhow::Result<Material> {
    let doc: serde_json::Value = serde_json::from_str(text.trim()).context(
        "the `credential` output group is read as JSON — pipe the tool's output through `jq` if \
         it prints something else",
    )?;
    let obj = doc.as_object().ok_or_else(|| {
        anyhow::anyhow!("the `credential` output group must be a JSON object of fields")
    })?;
    let mut fields = BTreeMap::new();
    for (k, v) in obj {
        // Expiry, by whichever of the two names the tool used. Not a field: it is
        // metadata about the material rather than part of it.
        if k == "expires_at" || k == "expires_in" {
            continue;
        }
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            // Nested structure is not a credential field. Silently flattening or
            // JSON-encoding it would hand a tool something that looks like a
            // token and is not.
            _ => continue,
        };
        fields.insert(k.clone(), s);
    }
    let expires_at = obj
        .get("expires_at")
        .or_else(|| obj.get("expires_in"))
        .map(parse_expiry)
        .transpose()?;
    Ok(Material {
        fields,
        files: BTreeMap::new(),
        expires_at,
    })
}

/// Two credentials on one target claiming the same environment variable.
///
/// Rejected rather than resolved: silently winning either way leaves one of the
/// two identities inoperative with nothing to see, and "which credential am I
/// actually using?" is the question this whole feature exists to answer.
fn check_env_collisions(consumer: &Addr, resolved: &[ResolvedCredential]) -> anyhow::Result<()> {
    let mut seen: BTreeMap<&str, &Addr> = BTreeMap::new();
    for rc in resolved {
        // Every presentation this credential could use, since which one wins is
        // not known until acquisition — and a collision that only appears on a
        // laptop is worse than one that appears everywhere.
        let presentations = rc
            .def
            .sources
            .iter()
            .filter_map(|s| s.present.as_ref())
            .chain(rc.def.present.as_ref());
        for present in presentations {
            for name in present.env.keys() {
                if let Some(other) = seen.insert(name, &rc.addr)
                    && other != &rc.addr
                {
                    anyhow::bail!(
                        "{consumer} declares both {other} and {}, and both present `{name}`. One \
                         would shadow the other — drop one of them, or change the variable on its \
                         presentation",
                        rc.addr
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_presented_file_lands_beside_the_workspace_and_never_inside_it() {
        // The whole reason for this path: output collection is rooted at the
        // workspace directory and packs every regular file it walks.
        let sandbox = Path::new("/s/__target_x");
        let dir = credential_dir(sandbox);
        assert_eq!(dir, Path::new("/s/__target_x/.heph/auth"));
        assert!(!dir.starts_with(sandbox.join("ws")));
    }

    #[test]
    fn a_credential_directory_is_named_by_its_full_address() {
        let a = Addr::new(
            hmodel::htpkg::PkgBuf::from("infra/auth"),
            "aws".to_string(),
            Default::default(),
        );
        let b = Addr::new(
            hmodel::htpkg::PkgBuf::from("svc/auth"),
            "aws".to_string(),
            Default::default(),
        );
        assert_ne!(
            sanitize_addr(&a),
            sanitize_addr(&b),
            "two credentials called `aws` in different packages must not collide"
        );
        assert!(!sanitize_addr(&a).contains('/'));
    }

    #[test]
    fn no_field_map_means_the_whole_text_is_one_field() {
        let m = material_from_text("  tok-123\n", &BTreeMap::new(), None).expect("parse");
        assert_eq!(m.fields.get("value").map(String::as_str), Some("tok-123"));
    }

    #[test]
    fn a_field_map_picks_dotted_json_paths() {
        let m = material_from_text(
            r#"{"data":{"token":"t"},"lease_duration":3600}"#,
            &BTreeMap::from([("token".to_string(), "data.token".to_string())]),
            Some("lease_duration"),
        )
        .expect("parse");
        assert_eq!(m.fields.get("token").map(String::as_str), Some("t"));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("epoch")
            .as_secs();
        let at = m.expires_at.expect("expiry");
        assert!(at > now + 3500 && at <= now + 3600, "got {at}");
    }

    #[test]
    fn a_number_is_a_duration_and_a_string_is_a_timestamp() {
        // Vendors do both, so the type is what tells them apart.
        let abs = parse_expiry(&serde_json::json!("2030-01-01T00:00:00Z")).expect("rfc3339");
        assert_eq!(abs, 1_893_456_000);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("epoch")
            .as_secs();
        let rel = parse_expiry(&serde_json::json!(60)).expect("duration");
        assert!(rel >= now + 59 && rel <= now + 61);
    }

    #[test]
    fn a_missing_field_names_what_the_source_did_yield() {
        let err = material_from_text(
            r#"{"AccessKeyId":"a"}"#,
            &BTreeMap::from([("secret".to_string(), "SecretAccessKey".to_string())]),
            None,
        )
        .expect_err("must fail");
        assert!(format!("{err:#}").contains("SecretAccessKey"), "{err:#}");
    }

    #[test]
    fn the_credential_output_group_becomes_fields_and_expiry() {
        let m = material_from_json_object(r#"{"token":"t","expires_in":120,"nested":{"a":1}}"#)
            .expect("parse");
        assert_eq!(m.fields.get("token").map(String::as_str), Some("t"));
        assert!(
            !m.fields.contains_key("expires_in"),
            "expiry is metadata about the material, not part of it"
        );
        assert!(
            !m.fields.contains_key("nested"),
            "nested structure is not a credential field"
        );
        assert!(m.expires_at.is_some());
    }

    #[test]
    fn template_rendering_resolves_fields_files_and_the_helper_argv() {
        let material = Material {
            fields: BTreeMap::from([("token".to_string(), "tok".to_string())]),
            files: BTreeMap::new(),
            expires_at: None,
        };
        let files = BTreeMap::from([("kc".to_string(), PathBuf::from("/s/.heph/auth/a/kc"))]);
        let args = |d: Dialect| vec!["__auth-helper".to_string(), d.as_str().to_string()];
        let cmd = PathBuf::from("/usr/local/bin/heph");
        assert_eq!(
            render("Bearer ${token}", &material, &files, None, &cmd, &args).expect("render"),
            "Bearer tok"
        );
        assert_eq!(
            render("${file:kc}", &material, &files, None, &cmd, &args).expect("render"),
            "/s/.heph/auth/a/kc"
        );
        assert_eq!(
            render(
                "${helper:command}",
                &material,
                &files,
                Some(Dialect::Kubernetes),
                &cmd,
                &args
            )
            .expect("render"),
            "/usr/local/bin/heph"
        );
        // An unknown field names what the source did yield, rather than
        // substituting an empty string.
        let err = render("${nope}", &material, &files, None, &cmd, &args).expect_err("must fail");
        assert!(format!("{err:#}").contains("token"), "{err:#}");
    }

    #[test]
    fn shell_join_quotes_what_a_shell_would_otherwise_split() {
        assert_eq!(
            shell_join(Path::new("/usr/bin/heph"), &["a".to_string()]),
            "/usr/bin/heph a"
        );
        assert_eq!(
            shell_join(
                Path::new("/Applications/My Tools/heph"),
                &["//auth:aws".to_string()]
            ),
            "'/Applications/My Tools/heph' //auth:aws"
        );
    }

    #[test]
    fn a_when_is_evaluated_against_this_host() {
        assert!(when_unmet(&When::Os("linux".to_string())).is_none() == cfg!(target_os = "linux"));
        assert!(when_unmet(&When::Env("HEPH_SURELY_UNSET_XYZ".to_string())).is_some());
    }

    #[test]
    fn two_credentials_claiming_one_variable_are_refused() {
        use hbuiltins::plugincredential::Presentation;
        let mk = |name: &str, var: &str| ResolvedCredential {
            addr: Addr::new(
                hmodel::htpkg::PkgBuf::from("auth"),
                name.to_string(),
                Default::default(),
            ),
            def: CredentialDef {
                sources: vec![],
                present: Some(Presentation {
                    env: BTreeMap::from([(var.to_string(), "${token}".to_string())]),
                    ..Presentation::default()
                }),
                ttl: None,
            },
        };
        let consumer = Addr::new(
            hmodel::htpkg::PkgBuf::from("svc"),
            "deploy".to_string(),
            Default::default(),
        );
        assert!(check_env_collisions(&consumer, &[mk("a", "TOKEN"), mk("b", "OTHER")]).is_ok());
        let err = check_env_collisions(&consumer, &[mk("a", "TOKEN"), mk("b", "TOKEN")])
            .expect_err("must fail");
        assert!(format!("{err:#}").contains("shadow"), "{err:#}");
    }

    #[test]
    fn an_env_variable_becomes_a_lowercase_field() {
        assert_eq!(env_field_name("GITHUB_TOKEN"), "github_token");
    }

    #[test]
    fn an_empty_variable_reads_as_unset() {
        // CI systems export an empty variable for a secret that was not
        // configured; treating that as present picks a source that cannot work.
        // SAFETY: single-threaded test, and the variable is unique to it.
        unsafe { std::env::set_var("HEPH_TEST_EMPTY_CRED", "") };
        assert!(host_env("HEPH_TEST_EMPTY_CRED").is_none());
        // SAFETY: as above.
        unsafe { std::env::remove_var("HEPH_TEST_EMPTY_CRED") };
    }
}
