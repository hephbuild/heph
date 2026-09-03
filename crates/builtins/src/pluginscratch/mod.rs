//! The `scratch` driver: declares a named, mutable, non-hermetic cache directory
//! that persists across runs and is shared by every target referencing it.
//!
//! A scratch directory is state carried between runs to make a build *faster*,
//! under one invariant that everything else here follows from:
//!
//! > A target must produce identical outputs whether its scratch directories are
//! > warm, cold, or absent. Losing one is always a slowdown and never a wrong
//! > answer.
//!
//! This driver is only the **declaration**. It has no inputs, no outputs, never
//! executes, and is never cached as a target — `parse` returns a def and `run` is
//! unreachable. The storage, locking and lineage that make a declaration mean
//! something live in the engine; see `docs/SCRATCH.md`.
//!
//! Declaring it as a target rather than inline at each use site is what makes
//! sharing work: the settings exist in exactly one place, so two consumers cannot
//! disagree about a slot's `access` or `version`, and the addr gives packages,
//! visibility and `heph query revdeps` for free.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse,
    targetdef::{CacheConfig, TargetDef},
};
use hplugin::htspec::Spec;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

pub const DRIVER_NAME: &str = "scratch";

/// How concurrent consumers of one slot are ordered.
///
/// The engine holds a keyed cross-process lock per slot; this decides whether a
/// consumer takes it for read or for write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Access {
    /// One consumer at a time. The safe default: a read-modify-write cache with
    /// no concurrency story of its own.
    Exclusive,
    /// Concurrent consumers permitted — "trust the tool". Only for caches that are
    /// safe under concurrent access *by construction*, which heph cannot check.
    ///
    /// Go's build cache is the motivating case: it is content-addressed and
    /// self-verifying, and concurrent access is what `go build -p N` already does.
    /// Forcing it to `Exclusive` would serialize an entire Go build.
    Shared,
}

impl Access {
    fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "exclusive" => Ok(Self::Exclusive),
            "shared" => Ok(Self::Shared),
            other => anyhow::bail!(
                "unknown scratch `access` {other:?} — expected \"exclusive\" (one consumer at a \
                 time) or \"shared\" (concurrent; only for a cache that is safe under concurrent \
                 access by construction)"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::Shared => "shared",
        }
    }
}

/// Config for a `scratch` target. `#[derive(Spec)]` provides the parser and the
/// LSP schema.
///
/// Every field describes **the cache**, never a particular consumer — which is
/// why referencing a scratch takes no configuration at all.
#[derive(Spec, Debug)]
struct ScratchSpec {
    /// Where the directory is mounted in a consuming target's sandbox, relative to
    /// that target's cwd. The same for every consumer: a cache's location is part
    /// of what it is, because tools bake absolute paths into their entries.
    #[spec(required)]
    path: String,
    /// Environment variable a consumer reads the directory's path from. Defaults
    /// to `SCRATCH_<NAME>`. Set it to the tool's own variable (`GOCACHE`,
    /// `CCACHE_DIR`) so consumers need no wiring of their own.
    #[spec(ty = ParamType::String)]
    env: Option<String>,
    /// `"exclusive"` (default) — one consumer at a time. `"shared"` — concurrent,
    /// for a cache that is safe under concurrent access by construction.
    #[spec(ty = ParamType::String)]
    access: Option<String>,
    /// Everything the contents depend on, beyond the declaring address —
    /// **the whole of it**. Two declarations sharing an addr share a directory
    /// if and only if they agree here.
    ///
    /// heph does not guess. If a cache is specific to the host, say so with
    /// `heph.core.os()` / `heph.core.arch()`; if it is specific to a toolchain
    /// release, a target triple, or a set of build tags, put those in too. An
    /// empty `version` (the default) declares a cache portable across every
    /// machine — which is right for a content-addressed blob store or a
    /// package-manager download cache, and wrong for a compiler cache.
    ///
    /// It doubles as the cache-bust handle: changing it yields a fresh, empty
    /// slot and invalidates no cached result.
    ///
    /// ```python
    /// version = heph.core.os() + "/" + heph.core.arch()   # host-specific
    /// version = goos + "/" + goarch + "/" + go_version    # target-specific
    /// version = ""                                        # portable
    /// ```
    version: String,
    /// Whether the slot may be pulled from the remote cache automatically and
    /// published to it by `heph tool scratch push`.
    remote: bool,
    /// Per-slot size cap (e.g. `"10GiB"`). Over it the slot is dropped whole
    /// rather than trimmed — heph cannot know which of a foreign tool's entries
    /// are hot, and guessing would degrade the cache while claiming to manage it.
    /// Omit to use the configured default.
    #[spec(ty = ParamType::String)]
    max_size: Option<String>,
}

/// A parsed scratch declaration.
///
/// Public because the engine reads these settings when a consumer references the
/// target — but note it reads them from the *spec config*, not from here: a
/// `raw_def` is opaque to the host by contract. This type is the driver's own
/// validated view, and the single place the parsing/validation rules live.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScratchDef {
    pub path: String,
    pub env: String,
    pub access: Access,
    pub version: String,
    pub remote: bool,
    pub max_size: Option<String>,
}

/// Default environment-variable name for a scratch called `name`.
///
/// Mirrors pluginexec's `OUT_<GROUP>`/`SRC_<GROUP>` convention: uppercase, every
/// character outside `[A-Z0-9_]` replaced with `_`. The `SCRATCH_` prefix means
/// the result is always a valid POSIX name even when the target name starts with
/// a digit.
pub fn default_env_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    out.push_str("SCRATCH_");
    for c in name.chars() {
        let u = c.to_ascii_uppercase();
        out.push(if u.is_ascii_alphanumeric() || u == '_' {
            u
        } else {
            '_'
        });
    }
    out
}

/// Reject a mount path that would escape the sandbox or collide with its root.
///
/// Checked on the *declaration* so it is reported once at the source rather than
/// once per consumer. A scratch mount is a symlink pointing out of the sandbox;
/// letting it be absolute or `..`-relative would put that symlink anywhere on the
/// machine, and letting it be `.` would replace the target's whole workspace.
fn validate_path(path: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("scratch `path` must not be empty");
    }
    if std::path::Path::new(path).is_absolute() {
        anyhow::bail!(
            "scratch `path` must be relative to the consuming target's cwd, got {path:?} — a \
             scratch is mounted inside the sandbox, not at an absolute location on the host"
        );
    }
    let mut depth = 0i32;
    for c in std::path::Path::new(path).components() {
        match c {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    anyhow::bail!(
                        "scratch `path` must stay inside the sandbox workspace, got {path:?} — \
                         `..` would place the mount outside it"
                    );
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => depth += 1,
            // Unreachable for a relative path, but a prefix/root here would mean
            // the same escape as an absolute path.
            _ => anyhow::bail!("scratch `path` must be a plain relative path, got {path:?}"),
        }
    }
    if depth == 0 {
        anyhow::bail!(
            "scratch `path` must name a directory inside the sandbox workspace, got {path:?} — \
             mounting at the workspace root would replace the target's whole tree"
        );
    }
    Ok(())
}

/// Parse and validate a scratch declaration straight from a target spec.
///
/// The engine calls this when a consumer references a scratch: it needs the
/// declaration's `path`, `env` and `access` to mount and lock the directory, and
/// a `raw_def` is opaque to the host by contract. Reading the spec config — which
/// *is* host-visible — through the same function the driver uses keeps one
/// implementation of the parsing and validation rules, so the host and the driver
/// can never disagree about what a declaration means.
pub fn parse_declaration(spec: &hplugin::provider::TargetSpec) -> anyhow::Result<ScratchDef> {
    let parsed = ScratchSpec::from(&spec.config).context("parse scratch config")?;
    ScratchDef::from_spec(parsed, &spec.addr.name)
}

impl ScratchDef {
    /// Parse and validate a declaration from its spec config.
    fn from_spec(spec: ScratchSpec, target_name: &str) -> anyhow::Result<Self> {
        validate_path(&spec.path)?;
        let access = spec
            .access
            .as_deref()
            .map(Access::parse)
            .transpose()?
            .unwrap_or(Access::Exclusive);
        let env = match spec.env {
            Some(e) if e.trim().is_empty() => {
                anyhow::bail!("scratch `env` must not be empty — omit it to get `SCRATCH_<NAME>`")
            }
            Some(e) => e,
            None => default_env_name(target_name),
        };
        Ok(Self {
            path: spec.path,
            env,
            access,
            version: spec.version,
            remote: spec.remote,
            max_size: spec.max_size,
        })
    }
}

pub struct Driver;

#[async_trait]
impl hplugin::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        ScratchSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let def = parse_declaration(&req.target_spec)
            .with_context(|| format!("scratch {}", req.target_spec.addr))?;

        // The def hash covers the declaration so that `heph inspect` and the
        // graph see a target that changes when its config does. It does NOT reach
        // any consumer's `hashin`: a scratch is referenced with `hashed: false`,
        // precisely because a target's outputs must be identical whether its
        // scratch is warm, cold, or absent. Folding this in would mean a `version`
        // bump rebuilt the world for no correctness gain.
        let mut h = Xxh3::new();
        h.update(req.target_spec.addr.format().as_bytes());
        h.update(def.path.as_bytes());
        h.update(def.env.as_bytes());
        h.update(def.access.as_str().as_bytes());
        h.update(def.version.as_bytes());
        h.update(&[def.remote as u8]);
        h.update(def.max_size.as_deref().unwrap_or("").as_bytes());
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: vec![],
                outputs: vec![],
                support_files: vec![],
                // Never cached as a target: there is nothing to cache. The
                // directory it names is managed by the engine's scratch store,
                // which is a different thing with a different lifetime.
                cache: CacheConfig::off(),
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
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        // Reachable, and not an error: resolving a declaration directly
        // (`heph run //build:gocache`) executes it like any other target. Zero
        // outputs is a supported shape — it is how a no-output gate target
        // registers a cache hit — so the right answer is to produce nothing and
        // succeed, not to fail.
        //
        // A declaration is inert by construction: no inputs to read, no outputs
        // to write, no command to spawn. Everything a scratch *does* happens in
        // the engine, on behalf of the targets that reference it.
        Ok(RunResponse::default())
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        self.run(req, ctoken).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::Value;
    use hmodel::htaddr::Addr;
    use hmodel::htpkg::PkgBuf;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn ctoken() -> hcore::hasync::StdCancellationToken {
        hcore::hasync::StdCancellationToken::new()
    }

    fn parse(pairs: &[(&str, Value)], name: &str) -> anyhow::Result<ScratchDef> {
        let spec = ScratchSpec::from(&cfg(pairs))?;
        ScratchDef::from_spec(spec, name)
    }

    #[test]
    fn defaults_are_the_conservative_ones() {
        let d = parse(&[("path", s(".cache/go-build"))], "gocache").expect("parse");
        assert_eq!(d.path, ".cache/go-build");
        // Exclusive, because a cache with no stated concurrency story gets the
        // safe treatment.
        assert_eq!(d.access, Access::Exclusive);
        // os_arch, because restoring a host-specific cache onto the wrong host is
        // the one mistake here that is not merely slow.
        assert!(!d.remote);
        assert_eq!(d.version, "");
        assert_eq!(d.max_size, None);
    }

    #[test]
    fn env_defaults_to_scratch_upper_name() {
        let d = parse(&[("path", s("c"))], "go-cache").expect("parse");
        assert_eq!(d.env, "SCRATCH_GO_CACHE");
    }

    #[test]
    fn env_can_be_the_tools_own_variable() {
        let d = parse(&[("path", s("c")), ("env", s("GOCACHE"))], "gocache").expect("parse");
        assert_eq!(d.env, "GOCACHE");
    }

    #[test]
    fn default_env_name_sanitizes_like_the_exec_group_convention() {
        assert_eq!(default_env_name("gocache"), "SCRATCH_GOCACHE");
        assert_eq!(default_env_name("go-cache"), "SCRATCH_GO_CACHE");
        assert_eq!(default_env_name("go.cache"), "SCRATCH_GO_CACHE");
        // Non-ASCII collapses rather than leaking bytes into an env var name.
        assert_eq!(default_env_name("café"), "SCRATCH_CAF_");
        // The prefix is what keeps a leading digit valid.
        assert_eq!(default_env_name("2fast"), "SCRATCH_2FAST");
    }

    #[test]
    fn access_parses_its_words() {
        let d = parse(&[("path", s("c")), ("access", s("shared"))], "n").expect("parse");
        assert_eq!(d.access, Access::Shared);
    }

    /// `version` is the *whole* of a slot's identity beyond its addr, so the
    /// default has to be the portable one. heph knowing the host and folding it
    /// in silently would be a guess about a cache it knows nothing about — and a
    /// closed set of guesses (os, arch) could never express a toolchain release,
    /// a target triple, or a set of build tags anyway.
    #[test]
    fn version_is_empty_by_default_and_carries_whatever_the_author_puts_there() {
        assert_eq!(parse(&[("path", s("c"))], "n").expect("parse").version, "");
        // Whatever it is, it is opaque — heph never parses or interprets it.
        let d = parse(
            &[("path", s("c")), ("version", s("linux/amd64/go1.27"))],
            "n",
        )
        .expect("parse");
        assert_eq!(d.version, "linux/amd64/go1.27");
    }

    #[test]
    fn an_unknown_access_says_what_the_options_mean() {
        let err = parse(&[("path", s("c")), ("access", s("concurrent"))], "n")
            .expect_err("unknown access must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("concurrent"), "{msg}");
        assert!(msg.contains("exclusive") && msg.contains("shared"), "{msg}");
    }

    #[test]
    fn path_is_required() {
        let err = ScratchSpec::from(&cfg(&[("version", s("1"))])).expect_err("path is required");
        assert!(format!("{err:#}").contains("path"));
    }

    /// The mount is a symlink out of the sandbox. An absolute path would let a
    /// BUILD file point it anywhere on the machine.
    #[test]
    fn an_absolute_path_is_rejected() {
        let err = parse(&[("path", s("/var/tmp/cache"))], "n").expect_err("absolute must fail");
        assert!(format!("{err:#}").contains("relative"));
    }

    #[test]
    fn a_path_escaping_the_workspace_is_rejected() {
        for p in ["../cache", "a/../../cache"] {
            let err = parse(&[("path", s(p))], "n").expect_err("escape must fail");
            assert!(
                format!("{err:#}").contains("inside the sandbox workspace"),
                "{p}"
            );
        }
    }

    /// `a/../b` never leaves the workspace, so it is allowed — the check is about
    /// escaping, not about spelling.
    #[test]
    fn a_path_that_re_enters_is_allowed() {
        let d = parse(&[("path", s("a/../b"))], "n").expect("parse");
        assert_eq!(d.path, "a/../b");
    }

    /// Mounting at the workspace root would replace the target's whole tree with
    /// the cache directory.
    #[test]
    fn mounting_at_the_workspace_root_is_rejected() {
        for p in [".", "./", "a/.."] {
            let err = parse(&[("path", s(p))], "n").expect_err("root mount must fail");
            assert!(
                format!("{err:#}").contains("replace the target's whole tree"),
                "{p}"
            );
        }
    }

    #[test]
    fn an_empty_env_is_rejected_rather_than_silently_defaulted() {
        let err = parse(&[("path", s("c")), ("env", s("  "))], "n").expect_err("empty env");
        assert!(format!("{err:#}").contains("omit it"));
    }

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("build"), name.to_string(), Default::default())
    }

    async fn parse_def(pairs: &[(&str, Value)], name: &str) -> anyhow::Result<TargetDef> {
        use hplugin::driver::Driver as _;
        let spec = hplugin::provider::TargetSpec {
            addr: addr(name),
            driver: DRIVER_NAME.to_string(),
            config: cfg(pairs),
            labels: vec![],
            transitive: Default::default(),
            approval: Default::default(),
        };
        let res = Driver
            .parse(
                ParseRequest {
                    request_id: "t".to_string(),
                    target_spec: Arc::new(spec),
                },
                &ctoken(),
            )
            .await?;
        Ok(res.target_def)
    }

    /// A declaration is inert: nothing to build, nothing to cache, nothing to
    /// materialize. Everything a scratch *does* is the engine's job.
    #[tokio::test]
    async fn a_declaration_has_no_inputs_outputs_or_cache() {
        let def = parse_def(&[("path", s("c"))], "gocache")
            .await
            .expect("parse");
        assert!(def.inputs.is_empty());
        assert!(def.outputs.is_empty());
        assert!(def.support_files.is_empty());
        assert!(!def.cache.enabled);
        assert!(!def.cache.remote_enabled);
        assert!(!def.transparent);
        assert!(!def.hash.is_empty());
    }

    #[tokio::test]
    async fn the_def_hash_moves_with_every_declared_field() {
        let baseline = parse_def(&[("path", s("c"))], "n")
            .await
            .expect("parse")
            .hash;
        for (field, value) in [
            ("version", s("v2")),
            ("access", s("shared")),
            ("env", s("GOCACHE")),
            ("remote", Value::Bool(true)),
            ("max_size", s("1GiB")),
        ] {
            let hash = parse_def(&[("path", s("c")), (field, value)], "n")
                .await
                .expect("parse")
                .hash;
            assert_ne!(hash, baseline, "def hash must move for `{field}`");
        }
    }

    /// Two declarations differing only in package are different caches; the addr
    /// is the identity, which is what gives packages namespacing for free.
    #[tokio::test]
    async fn the_addr_is_part_of_the_def_hash() {
        use hplugin::driver::Driver as _;
        let mk = |pkg: &str| {
            let spec = hplugin::provider::TargetSpec {
                addr: Addr::new(PkgBuf::from(pkg), "gocache".to_string(), Default::default()),
                driver: DRIVER_NAME.to_string(),
                config: cfg(&[("path", s("c"))]),
                labels: vec![],
                transitive: Default::default(),
                approval: Default::default(),
            };
            async move {
                Driver
                    .parse(
                        ParseRequest {
                            request_id: "t".to_string(),
                            target_spec: Arc::new(spec),
                        },
                        &ctoken(),
                    )
                    .await
                    .expect("parse")
                    .target_def
                    .hash
            }
        };
        assert_ne!(mk("go").await, mk("rust").await);
    }

    /// Resolving a declaration directly is legal — the engine executes any target
    /// asked for, and zero outputs is a supported shape. It must produce nothing
    /// and succeed.
    #[tokio::test]
    async fn running_a_declaration_produces_nothing_and_succeeds() {
        use hplugin::driver::Driver as _;
        let def = parse_def(&[("path", s("c"))], "gocache")
            .await
            .expect("parse");
        let req = RunRequest {
            request_id: &"t".to_string(),
            target: &def,
            tree_root_path: std::path::PathBuf::from("/"),
            inputs: vec![],
            hashin: "h",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: std::path::PathBuf::from("/tmp"),
            scratch: vec![],
        };
        let res = Driver
            .run(req, &ctoken())
            .await
            .expect("a declaration runs trivially");
        assert!(res.artifacts.is_empty());
        assert!(res.sandbox_cleanup.is_none());
    }
}
