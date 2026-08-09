//! The `js_install` driver: one hermetic download+verify+extract target per
//! third-party `(name, version, integrity)`, per
//! `ai-docs/js-plugin-plan.md`'s Hermeticity section.
//!
//! **Fetch strategy.** Unlike the Go plugin's thirdparty `download` target
//! (`crates/plugin-go/src/plugingo/thirdparty.rs`), which shells out to the
//! real `go mod download` (itself the module-proxy client, respecting
//! `GOPROXY`/auth), this driver fetches the registry tarball directly over
//! HTTPS (the same `reqwest` + explicit-hash-verify idiom `http_fetch`
//! already uses in this codebase) rather than shelling to `npm`/`pnpm`.
//! Reasoning, since the design doc's "same pattern as thirdparty.rs"
//! wording could be read either way:
//! - There is no hermetic npm/pnpm toolchain yet (Go's approach leans on the
//!   already-staged hermetic SDK from `toolchain.rs`; JS has no equivalent
//!   milestone built).
//! - The public npm registry protocol is a plain content-addressed HTTPS
//!   GET + SRI hash — exactly `http_fetch`'s own idiom, not a bespoke
//!   integration.
//! - **Known gap, TODO M2+:** this does not read `.npmrc` for private-registry
//!   auth tokens/scoped-registry overrides. A private-registry dependency
//!   fails today; shelling to a real, `.npmrc`-aware `npm`/`pnpm` binary (once
//!   one is hermetically provisioned, mirroring `toolchain.rs`) is the fix,
//!   tracked as part of the not-yet-built package-manager-toolchain
//!   milestone (the design doc's `pkgmanager=host` escape hatch).
//!
//! **Platform in the cache key.** `os`/`arch` are hashed (and carried as
//! addr args, see `thirdparty::thirdparty_addr`) for *every* third-party
//! package, not only ones the lockfile marks `os`/`cpu`-restricted. A plain
//! source npm package is byte-identical across platforms so this costs a
//! redundant cache entry per platform for the common case — accepted
//! deliberately (see `ai-docs/js-plugin-plan.md` Open Decision 1: "confirmed,
//! not optional") because the alternative (only hash platform for
//! `os`/`cpu`-restricted packages) misses the *other* native-dependency
//! shape: a package with no `os`/`cpu` restriction at all whose allow-listed
//! postinstall script compiles a native `.node` binding (node-gyp), which is
//! exactly as platform-sensitive despite carrying no restriction metadata.
//! Classifying "does this package need the platform axis" would have to
//! special-case that too — simpler and safer to always hash it, matching Go's
//! own precedent of unconditionally hashing `GOOS`/`GOARCH` into every
//! `_golist`/compile `Def` regardless of whether a given package's sources
//! are actually platform-conditional.
//!
//! **Lifecycle scripts.** Off by default, uniformly across both managers —
//! `Provider::get` (not this driver) decides `scripts_allowed` from the
//! provider's `allow_scripts` option and stamps it onto the target config,
//! so the driver itself stays a dumb, context-free executor. A
//! script-requiring package that isn't allow-listed fails **in `parse`**
//! (before any network fetch), naming the package — never a silent skip.
//! When allowed, the script is best-effort: run via the host's `sh`/PATH,
//! **not** sandboxed or declared-input the way the design doc's end state
//! calls for (`ai-docs/js-plugin-plan.md`'s "heph's own separate, explicit,
//! sandboxed action") — that needs a hermetic Node/script-runner sandbox
//! that does not exist yet. TODO M2+: sandbox this properly; until then it
//! is flagged loudly (`tracing::warn!`) every time it runs. Its stdout/stderr
//! are captured, never inherited from heph's own process — an arbitrary
//! package's script writing straight to the real terminal would corrupt the
//! TUI, which owns the alternate screen for the whole run; a captured
//! failure's output is folded into the error instead.
//!
//! **Empty `integrity` installs unverified, by explicit product decision.**
//! A real npm `package-lock.json` can have a `packages` entry with no
//! `integrity`/`resolved` at all — not a heph bug, but a known npm CLI bug
//! (npm/cli#4263, #4460, #6301): `npm install`, unlike `npm ci`, can satisfy
//! a package from its local cache and strip these fields from an existing
//! entry instead of repopulating them. Rather than block every affected
//! package's install on a lockfile heph didn't write and can't fix, an
//! empty `integrity` skips `verify_integrity` entirely and extracts
//! whatever the registry returns — a deliberate hermeticity trade-off (the
//! cache key still changes if the lockfile is later regenerated with a real
//! hash, but two unverified fetches of the same URL on different days could
//! in principle diverge with no error). `fetch_and_extract`'s
//! `tracing::warn!` is the only trace this leaves.

use anyhow::Context as _;
use async_trait::async_trait;
use base64::Engine as _;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{CacheConfig, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse,
};
use hplugin::htspec::Spec;
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::hash::{Hash, Hasher};
use std::path::Path as StdPath;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// Config for a `js_install` target. Entirely engine-generated by the `js`
/// provider's `Provider::get` (see `pluginjs::deps`/`pluginjs::thirdparty`) —
/// never authored by hand in a BUILD file.
#[derive(Spec)]
struct JsInstallSpec {
    /// Package name, e.g. `"lodash"` or `"@esbuild/darwin-arm64"`.
    #[spec(required)]
    name: String,
    /// Exact resolved version.
    #[spec(required)]
    version: String,
    /// Subresource-Integrity hash (`"sha512-…"` / `"sha1-…"`) from the
    /// lockfile, verified against the fetched tarball's bytes before
    /// extraction.
    #[spec(required)]
    integrity: String,
    /// Tarball URL — the lockfile's own `resolved` field (npm), or the
    /// default npm registry convention URL derived from `name`/`version`
    /// (pnpm's common case for a plain registry dependency).
    #[spec(required)]
    resolved: String,
    /// Building machine's OS (canonical Go/OCI naming) — part of the cache
    /// key, see module docs.
    #[spec(required)]
    os: String,
    /// Building machine's architecture (canonical Go/OCI naming) — part of
    /// the cache key, see module docs.
    #[spec(required)]
    arch: String,
    /// Whether the package declares an install/preinstall/postinstall
    /// lifecycle script (from the lockfile: npm's `hasInstallScript` /
    /// pnpm's `requiresBuild`).
    has_install_script: bool,
    /// Whether heph's `allow_scripts` provider option permits running that
    /// script for this exact package. Computed by `Provider::get`, not by
    /// this driver — it stays a context-free executor of what it's told.
    scripts_allowed: bool,
}

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
struct JsInstallDef {
    name: String,
    version: String,
    integrity: String,
    resolved: String,
    os: String,
    arch: String,
    has_install_script: bool,
    scripts_allowed: bool,
}

/// Bump to invalidate every cached `js_install` artifact whenever the
/// on-disk artifact layout (extracted tree shape, stripped tarball root,
/// lifecycle-script handling) changes.
const JS_INSTALL_FORMAT_VERSION: u32 = 1;

pub struct JsInstallDriver;

impl JsInstallDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsInstallDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for JsInstallDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "js_install".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        JsInstallSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec =
            JsInstallSpec::from(&req.target_spec.config).context("parse js_install config")?;

        // Fail loudly, before any network fetch: never silently skip a
        // required lifecycle script (`.claude/rust.md`/memory: "fail or fix,
        // never ignore"). See module docs for the allowlist's shape.
        if spec.has_install_script && !spec.scripts_allowed {
            anyhow::bail!(
                "js_install: `{}@{}` declares an install/preinstall/postinstall lifecycle \
                 script, which heph runs with scripts disabled by default. Add `{}` (or \
                 `{}@{}`) to the js provider's `allow_scripts` option to permit it explicitly, \
                 or the package cannot be installed hermetically.",
                spec.name,
                spec.version,
                spec.name,
                spec.name,
                spec.version
            );
        }

        // Gate `scripts_allowed` to `has_install_script` before it's hashed:
        // the allowlist only ever affects behavior (whether `run()` executes
        // a script) when a script exists at all, so pre-emptively adding a
        // package to `allow_scripts` before it ever declares one must not
        // bust its cache for zero behavior change.
        let run_script = spec.has_install_script && spec.scripts_allowed;

        let def = JsInstallDef {
            name: spec.name,
            version: spec.version,
            integrity: spec.integrity,
            resolved: spec.resolved,
            os: spec.os,
            arch: spec.arch,
            has_install_script: spec.has_install_script,
            scripts_allowed: run_script,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("js_install_{}", addr.format())
            });
            JS_INSTALL_FORMAT_VERSION.hash(&mut h);
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        let pkg_str = addr.package.as_str().to_string();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                // No inputs: the bytes come from the network, not from
                // other targets — same as `http_fetch`.
                inputs: vec![],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::DirPath(pkg_str),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                support_files: vec![],
                // Content-addressed (pinned by `integrity`) — safe to share
                // across the local and remote cache tiers, UNLESS an
                // allow-listed lifecycle script actually runs: `run_script`
                // then shells out to the host's ambient `sh`/`PATH`/compiler/
                // Node headers (see `run_lifecycle_scripts`'s module docs),
                // none of which are in this hash. Caching that output
                // (locally or remotely) under a key that never covers the
                // toolchain that produced it is exactly the "silent wrong
                // build" hermeticity fixed a machine-A-built native addon
                // being served, cache-hit, to machine B with a different
                // toolchain. Disabled entirely (not just remote) since a
                // same-machine toolchain change — a Node/compiler upgrade —
                // wouldn't bust this hash either. TODO M2+: once the
                // script's own toolchain is hermetically pinned/declared,
                // re-enable caching for this case.
                cache: if run_script {
                    CacheConfig::off()
                } else {
                    CacheConfig::on(true)
                },
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
        let def = req.request.target.def_de::<JsInstallDef>();
        let dest_dir = req.sandbox_pkg_dir.clone();

        let resolved = def.resolved.clone();
        let integrity = def.integrity.clone();
        let name = def.name.clone();
        let version = def.version.clone();
        let run_script = def.has_install_script && def.scripts_allowed;

        // Download + verify + extract (+ maybe run an allow-listed script) is
        // blocking IO/CPU; keep it off the async runtime, same as
        // `http_fetch::fetch`. Cancellation races the join handle rather than
        // interrupting mid-syscall.
        let work = tokio::task::spawn_blocking(move || {
            fetch_and_extract(&resolved, &integrity, &dest_dir)?;
            if run_script {
                run_lifecycle_scripts(&dest_dir, &name).with_context(|| {
                    format!("run allow-listed lifecycle scripts for {name}@{version}")
                })?;
            }
            anyhow::Ok(())
        });

        let fetch = async {
            work.await
                .context("js_install download task panicked")?
                .with_context(|| format!("install {}@{}", def.name, def.version))
        };

        tokio::select! {
            r = fetch => r?,
            () = ctoken.cancelled() => anyhow::bail!(
                "js_install: {}@{} cancelled", def.name, def.version
            ),
        }

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Compare `bytes`' hash against a Subresource-Integrity string
/// (`"sha512-BASE64"`, optionally several space-separated entries — SRI
/// permits multiple algorithms per resource). At least one recognized
/// (`sha512`/`sha1`) entry must match; an unrecognized-only *non-empty*
/// integrity string fails closed rather than silently passing.
///
/// An *empty* `integrity` is a distinct, deliberately-permitted case (see
/// `fetch_and_extract`'s doc) and never reaches this function — callers
/// route it to unverified install before calling this at all.
fn verify_integrity(bytes: &[u8], integrity: &str) -> anyhow::Result<()> {
    let mut checked_any = false;
    for entry in integrity.split_whitespace() {
        let Some((algo, expected_b64)) = entry.split_once('-') else {
            continue;
        };
        let got = match algo {
            "sha512" => base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes)),
            "sha1" => base64::engine::general_purpose::STANDARD.encode(Sha1::digest(bytes)),
            _ => continue,
        };
        checked_any = true;
        if got != expected_b64 {
            anyhow::bail!(
                "integrity mismatch ({algo}): expected {expected_b64}, got {got} — the fetched \
                 tarball does not match the lockfile"
            );
        }
    }
    anyhow::ensure!(
        checked_any,
        "no recognized integrity algorithm in {integrity:?} (expected a sha512- or sha1- entry)"
    );
    Ok(())
}

/// Strip a tarball entry's own top-level directory (npm publishes every
/// tarball with a `package/` root: `package/foo.js` → `foo.js`) and reject
/// any result that would escape the extraction root via a `..` component
/// (zip-slip) — `None` for a rejected or now-empty (the top-level dir entry
/// itself) path.
fn strip_tarball_root(path: &StdPath) -> anyhow::Result<Option<std::path::PathBuf>> {
    let rel: std::path::PathBuf = path.components().skip(1).collect();
    if rel.as_os_str().is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        !rel.components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "tarball entry {path:?} escapes the extraction root via `..`"
    );
    Ok(Some(rel))
}

/// Verify that `path` (an ancestor directory that must exist by the time
/// this runs — either it pre-existed or [`ensure_extraction_parent_is_safe`]
/// just created it) resolves, once symlinks are followed, to somewhere
/// inside `dest_dir`. Mirrors `tar::Entry::unpack_in`'s own
/// `validate_inside_dst` — unavailable to call directly here since
/// extracting with npm's `package/` root already stripped requires the
/// lower-level `Entry::unpack`, which bypasses all of `unpack_in`'s own
/// safety checks (see [`extract_tarball`]'s doc comment).
fn validate_inside_dest(dest_dir: &StdPath, path: &StdPath) -> anyhow::Result<()> {
    let canon_path = path
        .canonicalize()
        .with_context(|| format!("canonicalize {path:?}"))?;
    let canon_dest = dest_dir
        .canonicalize()
        .with_context(|| format!("canonicalize {dest_dir:?}"))?;
    anyhow::ensure!(
        canon_path.starts_with(&canon_dest),
        "tarball entry escapes the extraction root {dest_dir:?}: {path:?} resolves outside it \
         (an earlier entry likely placed a symlink here) — refusing to extract"
    );
    Ok(())
}

/// Create `parent` (an already-root-stripped ancestor-directory chain under
/// `dest_dir`) safely, then verify it resolves inside `dest_dir` once
/// canonicalized — this is what stops a tar-slip: an earlier tar entry
/// placing a symlink at a path a later entry's parent directory expects,
/// silently redirecting that later entry's write outside `dest_dir` (a
/// symlink target is never itself path-checked — only the *directory
/// components a later write walks through* are, exactly as upstream `tar`
/// does via `ensure_dir_created`/`validate_inside_dst`).
///
/// Critically, the final `validate_inside_dest` call below runs
/// unconditionally, whether `parent` needed creating or already existed —
/// a plain `create_dir_all` through a pre-existing symlink ancestor succeeds
/// silently (that's the whole exploit), so the loop above alone is not
/// enough; every call must re-check the immediate parent itself.
fn ensure_extraction_parent_is_safe(dest_dir: &StdPath, parent: &StdPath) -> anyhow::Result<()> {
    let mut ancestor = parent;
    let mut to_create = Vec::new();
    while std::fs::symlink_metadata(ancestor).is_err() {
        to_create.push(ancestor);
        match ancestor.parent() {
            Some(p) => ancestor = p,
            None => break,
        }
    }
    for dir in to_create.into_iter().rev() {
        if let Some(p) = dir.parent() {
            validate_inside_dest(dest_dir, p)?;
        }
        std::fs::create_dir_all(dir).with_context(|| format!("create dir {dir:?}"))?;
    }
    validate_inside_dest(dest_dir, parent)
}

/// Extract a gzip'd tarball into `dest_dir`. See [`strip_tarball_root`] for
/// the root-stripping/zip-slip-rejection rule applied to each entry's own
/// *path*, and [`ensure_extraction_parent_is_safe`] for the complementary
/// guard against a symlink *target* escape: a tar entry's path can only
/// smuggle `..` in its own name (rejected above), but a Symlink-type entry's
/// *link target* can point anywhere (`../../etc`, an absolute path) with
/// nothing in the entry's own path naming it — a later entry that writes
/// through that symlink would otherwise land outside `dest_dir` even though
/// no single entry's path ever contained `..`. `tar::Entry::unpack_in` (the
/// crate's own safe, high-level API) guards exactly this, but requires
/// unpacking each entry's path as recorded — incompatible with stripping
/// npm's `package/` root per entry, so that guard is reimplemented here
/// against the already-stripped destination instead of skipped.
fn extract_tarball(bytes: &[u8], dest_dir: &StdPath) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("create install dest dir {dest_dir:?}"))?;
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("read tarball entries")? {
        let mut entry = entry.context("read tarball entry")?;
        let path = entry.path().context("tarball entry path")?.into_owned();
        let Some(rel) = strip_tarball_root(&path)? else {
            continue;
        };
        let dest = dest_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            ensure_extraction_parent_is_safe(dest_dir, parent)
                .with_context(|| format!("unpack tarball entry to {dest:?}"))?;
        }
        // A directory entry never gets the archive's own recorded mode —
        // confirmed live: a real npm tarball (pngjs@7.0.0) ships a
        // directory entry (`lib/`) with mode 0666 (no execute bit), and
        // `Entry::unpack` applies that chmod immediately on this entry,
        // before the *next* entry (a file inside that directory) can be
        // created — Permission denied, since Unix file creation is gated
        // on the parent directory's execute bit, not the child's own mode.
        // Upstream `tar::Archive::unpack` avoids exactly this by deferring
        // every directory entry's permissions to the very end of
        // extraction (tar-rs#242); that high-level API is unusable here
        // (it can't strip npm's `package/` root per-entry, which this loop
        // must do), so instead directories simply never get their
        // archive-recorded mode narrowed at all — same discipline this
        // codebase's own `hartifactcontent::unpack` already follows
        // (extraction only ever adds permissions to a file, e.g. `+x`;
        // never narrows one from untrusted archive data).
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&dest).with_context(|| format!("create dir {dest:?}"))?;
            continue;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("unpack tarball entry to {dest:?}"))?;
    }
    Ok(())
}

/// Fetch `url`'s tarball and extract it to `dest_dir`. When `integrity` is
/// non-empty, the download is verified against it before a single byte is
/// extracted, same as always. When `integrity` is *empty* — the lockfile
/// itself never recorded one for this package (a real, observed npm shape:
/// `npm install`, unlike `npm ci`, can satisfy a package from its local
/// cache and strip `resolved`/`integrity` from an existing
/// `package-lock.json` entry instead of repopulating them — npm/cli#4263,
/// #4460, #6301, not a heph bug) — there is nothing to verify against, and
/// by explicit product decision this installs unverified rather than
/// blocking the build on a lockfile heph didn't write and can't fix. The
/// `tracing::warn!` is the only trace this leaves; nothing else in the
/// pipeline flags an unverified install once this returns.
fn fetch_and_extract(url: &str, integrity: &str, dest_dir: &StdPath) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .context("build http client")?;
    let bytes = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .bytes()
        .with_context(|| format!("read body of {url}"))?;

    if integrity.is_empty() {
        tracing::warn!(
            url,
            "js_install: no integrity recorded for this package in the lockfile (a known npm bug, not a heph one — see fetch_and_extract's doc); installing unverified"
        );
    } else {
        verify_integrity(&bytes, integrity)
            .with_context(|| format!("verify integrity of {url}"))?;
    }
    extract_tarball(&bytes, dest_dir).with_context(|| format!("extract tarball from {url}"))
}

/// Best-effort execution of an allow-listed package's own
/// `preinstall`/`install`/`postinstall` scripts (npm's lifecycle order), via
/// the host's `sh` and `PATH`. **Not hermetic or sandboxed** — see module
/// docs' TODO. Only reached when `Provider::get` has already stamped
/// `scripts_allowed = true` for this exact package.
fn run_lifecycle_scripts(dest_dir: &StdPath, name: &str) -> anyhow::Result<()> {
    let package_json = dest_dir.join("package.json");
    if !package_json.is_file() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&package_json)
        .with_context(|| format!("reading {package_json:?}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {package_json:?}"))?;
    let Some(scripts) = value.get("scripts").and_then(serde_json::Value::as_object) else {
        return Ok(());
    };

    for key in ["preinstall", "install", "postinstall"] {
        let Some(script) = scripts.get(key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        tracing::warn!(
            package = name,
            script = key,
            "js_install: running allow-listed lifecycle script via the host shell — not yet \
             sandboxed (TODO M2+); see driver_install.rs module docs"
        );
        // Captured, not inherited: `Command`'s default stdio is the
        // parent's own — confirmed live, an arbitrary package's lifecycle
        // script writing straight to the real terminal corrupts heph's TUI
        // (which owns the alternate screen the whole run). Capturing here
        // also means a failing script's own output — often the only real
        // diagnostic for *why* a native build failed — actually reaches
        // the error below instead of vanishing into whatever the terminal
        // happened to be showing at the time.
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dest_dir)
            .output()
            .with_context(|| format!("spawn `{key}` script for {name}"))?;
        anyhow::ensure!(
            output.status.success(),
            "`{key}` script for {name} exited with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::Addr;
    use hmodel::htpkg::PkgBuf;
    use hplugin::provider::TargetSpec;
    use sha2::{Digest, Sha512};
    use std::collections::HashMap;

    fn driver() -> JsInstallDriver {
        JsInstallDriver::new()
    }

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn config(extra: &[(&str, Value)]) -> HashMap<String, Value> {
        let mut c: HashMap<String, Value> = HashMap::from([
            ("name".to_string(), Value::String("lodash".to_string())),
            ("version".to_string(), Value::String("4.17.21".to_string())),
            (
                "integrity".to_string(),
                Value::String("sha512-abc".to_string()),
            ),
            (
                "resolved".to_string(),
                Value::String("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string()),
            ),
            ("os".to_string(), Value::String("linux".to_string())),
            ("arch".to_string(), Value::String("amd64".to_string())),
        ]);
        for (k, v) in extra {
            c.insert((*k).to_string(), v.clone());
        }
        c
    }

    fn make_parse_request(extra: &[(&str, Value)]) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("@heph/js/thirdparty/lodash@4.17.21"),
                    "js_install".to_string(),
                    Default::default(),
                ),
                driver: "js_install".to_string(),
                config: config(extra),
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn driver_name_is_js_install() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "js_install");
    }

    #[tokio::test]
    async fn parse_missing_required_field_errors() {
        let ct = ctoken();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("@heph/js/thirdparty/lodash@4.17.21"),
                    "js_install".to_string(),
                    Default::default(),
                ),
                driver: "js_install".to_string(),
                ..Default::default()
            }),
        };
        assert!(driver().parse(req, &ct).await.is_err());
    }

    #[tokio::test]
    async fn parse_unallowlisted_install_script_fails_loudly_naming_package() {
        let ct = ctoken();
        let req = make_parse_request(&[("has_install_script", Value::Bool(true))]);
        let err = driver()
            .parse(req, &ct)
            .await
            .err()
            .expect("unallowlisted install script must fail parse");
        let msg = format!("{err:#}");
        assert!(msg.contains("lodash"), "must name the package: {msg}");
        assert!(
            msg.contains("allow_scripts"),
            "must point at the fix: {msg}"
        );
    }

    #[tokio::test]
    async fn parse_allowlisted_install_script_succeeds() {
        let ct = ctoken();
        let req = make_parse_request(&[
            ("has_install_script", Value::Bool(true)),
            ("scripts_allowed", Value::Bool(true)),
        ]);
        driver().parse(req, &ct).await.unwrap();
    }

    #[tokio::test]
    async fn parse_no_install_script_needs_no_allowlisting() {
        let ct = ctoken();
        let req = make_parse_request(&[]);
        driver().parse(req, &ct).await.unwrap();
    }

    #[tokio::test]
    async fn parse_hash_changes_when_integrity_changes() {
        let ct = ctoken();
        let resp_a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let resp_b = driver()
            .parse(
                make_parse_request(&[("integrity", Value::String("sha512-different".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(resp_a.target_def.hash, resp_b.target_def.hash);
    }

    #[tokio::test]
    async fn parse_hash_stable_across_identical_parses() {
        let ct = ctoken();
        let resp_a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let resp_b = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(resp_a.target_def.hash, resp_b.target_def.hash);
    }

    #[tokio::test]
    async fn parse_hash_changes_per_platform() {
        let ct = ctoken();
        let resp_linux = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let resp_darwin = driver()
            .parse(
                make_parse_request(&[
                    ("os", Value::String("darwin".to_string())),
                    ("arch", Value::String("arm64".to_string())),
                ]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(resp_linux.target_def.hash, resp_darwin.target_def.hash);
    }

    #[tokio::test]
    async fn parse_declares_no_inputs_and_one_dir_output() {
        let ct = ctoken();
        let resp = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert!(resp.target_def.inputs.is_empty());
        assert_eq!(resp.target_def.outputs.len(), 1);
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::DirPath(p) if p == "@heph/js/thirdparty/lodash@4.17.21"
        ));
    }

    #[tokio::test]
    async fn parse_caches_locally_and_remotely() {
        let ct = ctoken();
        let resp = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
    }

    /// Hermeticity: an allow-listed lifecycle script executes against the
    /// ambient host toolchain (not hashed) — its output must never be
    /// cached, locally or remotely, or a cache hit would silently serve one
    /// machine's native build to another. See module docs / the `cache`
    /// field's comment in `parse()`.
    #[tokio::test]
    async fn parse_disables_all_caching_when_an_allowlisted_script_will_run() {
        let ct = ctoken();
        let req = make_parse_request(&[
            ("has_install_script", Value::Bool(true)),
            ("scripts_allowed", Value::Bool(true)),
        ]);
        let resp = driver().parse(req, &ct).await.unwrap();
        assert!(
            !resp.target_def.cache.enabled,
            "must not cache locally either: a same-machine toolchain change wouldn't bust this hash"
        );
        assert!(!resp.target_def.cache.remote_enabled);
    }

    /// A package with no install script still caches normally even once
    /// allow-listed pre-emptively — the allowlist alone changes nothing
    /// observable, so it must not disable caching either.
    #[tokio::test]
    async fn parse_still_caches_when_allowlisted_but_no_install_script() {
        let ct = ctoken();
        let req = make_parse_request(&[("scripts_allowed", Value::Bool(true))]);
        let resp = driver().parse(req, &ct).await.unwrap();
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
    }

    /// Pre-emptively adding a package to `allow_scripts` before it ever
    /// declares an install script is a no-op change in observable behavior,
    /// so it must not bust the package's cache key either.
    #[tokio::test]
    async fn parse_hash_unaffected_by_preemptive_allowlisting_without_install_script() {
        let ct = ctoken();
        let resp_a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let resp_b = driver()
            .parse(
                make_parse_request(&[("scripts_allowed", Value::Bool(true))]),
                &ct,
            )
            .await
            .unwrap();
        assert_eq!(
            resp_a.target_def.hash, resp_b.target_def.hash,
            "allow-listing a package before it ever declares an install script must not change \
             its cache key"
        );
    }

    // ---- integrity / extraction ----

    #[test]
    fn verify_integrity_accepts_matching_sha512() {
        let bytes = b"hello world";
        let b64 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes));
        verify_integrity(bytes, &format!("sha512-{b64}")).unwrap();
    }

    #[test]
    fn verify_integrity_rejects_mismatch() {
        let bytes = b"hello world";
        let err = verify_integrity(bytes, "sha512-not-the-real-hash").unwrap_err();
        assert!(format!("{err:#}").contains("integrity mismatch"));
    }

    #[test]
    fn verify_integrity_rejects_unrecognized_algorithm_only() {
        let err = verify_integrity(b"x", "md5-abc").unwrap_err();
        assert!(format!("{err:#}").contains("no recognized integrity algorithm"));
    }

    fn make_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("package/{path}"), *contents)
                .expect("append tar entry");
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar_bytes).expect("gzip write");
        gz.finish().expect("gzip finish")
    }

    #[test]
    fn extract_tarball_strips_package_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = make_tarball(&[("index.js", b"module.exports = 1;")]);
        extract_tarball(&bytes, dir.path()).expect("extract");
        assert!(dir.path().join("index.js").is_file());
    }

    /// Reproduces a real published tarball's exact shape: `pngjs@7.0.0`
    /// ships an explicit `package/lib` directory entry with mode `0666` —
    /// no execute bit — followed by files inside it. A directory missing
    /// its execute bit can't have anything *created* inside it (Unix file
    /// creation is gated on the parent directory's execute bit, not the
    /// child's own mode), so if `extract_tarball` ever applies that
    /// directory entry's own recorded mode, the very next entry
    /// (`lib/bitmapper.js`) fails to unpack with a permission error —
    /// confirmed live, this is not a hypothetical shape.
    #[test]
    fn extract_tarball_ignores_restrictive_mode_on_a_directory_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut builder = tar::Builder::new(Vec::new());

        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_size(0);
        dir_header.set_mode(0o666);
        dir_header.set_cksum();
        builder
            .append_data(&mut dir_header, "package/lib", std::io::empty())
            .expect("append directory entry");

        let contents = b"module.exports = function bitmap() {};";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(contents.len() as u64);
        file_header.set_mode(0o644);
        file_header.set_cksum();
        builder
            .append_data(&mut file_header, "package/lib/bitmapper.js", &contents[..])
            .expect("append file entry inside the restrictive directory");

        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar_bytes).expect("gzip write");
        let bytes = gz.finish().expect("gzip finish");

        extract_tarball(&bytes, dir.path()).expect(
            "a restrictive directory-entry mode from the archive must never block extracting \
             the files inside it",
        );
        assert_eq!(
            std::fs::read(dir.path().join("lib/bitmapper.js")).expect("read extracted file"),
            contents
        );
    }

    /// A symlink entry, followed by a second entry that writes *through*
    /// it — the two-step tar-slip: neither entry's own *path* ever contains
    /// `..` (so [`strip_tarball_root`]'s guard, which only inspects paths,
    /// never fires), but the first entry's *link target* points outside
    /// `dest_dir` entirely, and the second entry's write would land there
    /// if `extract_tarball` didn't reject it. This is the exact escape
    /// `code-quality`'s review reproduced against `Entry::unpack`.
    fn make_symlink_escape_tarball(
        link_name: &str,
        link_target: &str,
        file_name: &str,
        contents: &[u8],
    ) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_size(0);
        link_header.set_mode(0o777);
        builder
            .append_link(&mut link_header, link_name, link_target)
            .expect("append symlink entry");

        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(contents.len() as u64);
        file_header.set_mode(0o644);
        file_header.set_cksum();
        builder
            .append_data(&mut file_header, file_name, contents)
            .expect("append file entry");

        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar_bytes).expect("gzip write");
        gz.finish().expect("gzip finish")
    }

    #[test]
    fn extract_tarball_rejects_symlink_then_write_escape() {
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let escape_dir = tempfile::tempdir().expect("tempdir");
        let escape_target = escape_dir.path().to_path_buf();

        let bytes = make_symlink_escape_tarball(
            "package/evil",
            escape_target.to_str().expect("utf8 tempdir path"),
            "package/evil/marker.txt",
            b"pwned",
        );

        let err = extract_tarball(&bytes, dest_dir.path())
            .expect_err("a write through an escaping symlink ancestor must be rejected");
        assert!(format!("{err:#}").contains("escapes"), "{err:#}");
        assert!(
            !escape_target.join("marker.txt").exists(),
            "the write must not land outside dest_dir via the symlink"
        );
    }

    // `tar::Builder` itself refuses to write a `..`-containing path (it
    // validates at append time), so a zip-slip archive can't be constructed
    // through the same helper `extract_tarball_strips_package_root` uses
    // above — the rejection is exercised directly against the stripping
    // helper instead, which is what actually guards `extract_tarball`.
    #[test]
    fn strip_tarball_root_rejects_path_traversal() {
        let err = strip_tarball_root(StdPath::new("package/../../etc/passwd")).unwrap_err();
        assert!(format!("{err:#}").contains("escapes"), "{err:#}");
    }

    #[test]
    fn strip_tarball_root_strips_leading_component() {
        let rel = strip_tarball_root(StdPath::new("package/lib/index.js"))
            .expect("strip")
            .expect("non-empty");
        assert_eq!(rel, StdPath::new("lib/index.js"));
    }

    #[test]
    fn strip_tarball_root_top_level_entry_is_none() {
        assert!(
            strip_tarball_root(StdPath::new("package"))
                .expect("strip")
                .is_none()
        );
    }

    // ---- fetch_and_extract over a real loopback HTTP server ----
    //
    // Mirrors `crates/e2e/tests/http_fetch.rs`'s own loopback-server pattern
    // (`http_fetch` being the closest existing precedent for "download +
    // verify" driver behavior). `js_install` has no `crates/e2e` wiring yet
    // (the `js` provider isn't registered there — an explicit, tracked
    // deferral, see this task's final report), so this exercises the same
    // fetch→verify→extract pipeline `run()` calls directly, in-crate,
    // against a real socket rather than only unit-testing its pieces in
    // isolation.

    /// A one-shot loopback HTTP server serving `body` once. See
    /// `http_fetch.rs::serve` for the identical pattern. Read/write errors on
    /// the accepted socket are deliberately ignored (best-effort test
    /// plumbing, not production code — same exemption `http_fetch.rs` itself
    /// takes via its own file-level `#![expect(...)]`).
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test plumbing, not production code — same exemption http_fetch.rs takes"
    )]
    fn serve_once(body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/pkg.tgz", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut sock, &mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
            let _ = std::io::Write::write_all(&mut sock, &body);
        });
        url
    }

    #[test]
    fn fetch_and_extract_downloads_verifies_and_extracts_over_http() {
        let tarball = make_tarball(&[("index.js", b"module.exports = 1;")]);
        let sha512_b64 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball));
        let url = serve_once(tarball);
        let dir = tempfile::tempdir().expect("tempdir");

        fetch_and_extract(&url, &format!("sha512-{sha512_b64}"), dir.path())
            .expect("fetch, verify, and extract must all succeed");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("index.js")).expect("read extracted file"),
            "module.exports = 1;"
        );
    }

    #[test]
    fn fetch_and_extract_rejects_checksum_mismatch_over_http() {
        let tarball = make_tarball(&[("index.js", b"module.exports = 1;")]);
        let url = serve_once(tarball);
        let dir = tempfile::tempdir().expect("tempdir");

        let err = fetch_and_extract(&url, "sha512-not-the-real-hash", dir.path())
            .expect_err("a checksum mismatch must fail, not silently extract");
        assert!(format!("{err:#}").contains("integrity mismatch"), "{err:#}");
        assert!(
            !dir.path().join("index.js").exists(),
            "no bytes may land on disk when integrity verification fails"
        );
    }

    #[test]
    fn fetch_and_extract_installs_unverified_when_integrity_is_empty() {
        // The lockfile-omitted-integrity shape (npm/cli#4263) — see
        // `fetch_and_extract`'s doc. By explicit product decision this must
        // still succeed, extracting whatever the server returns, not error
        // the way a *non-empty but unrecognized* integrity string does.
        let tarball = make_tarball(&[("index.js", b"module.exports = 1;")]);
        let url = serve_once(tarball);
        let dir = tempfile::tempdir().expect("tempdir");

        fetch_and_extract(&url, "", dir.path())
            .expect("empty integrity must install unverified, not error");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("index.js")).expect("read extracted file"),
            "module.exports = 1;"
        );
    }

    // ---- run_lifecycle_scripts ----
    //
    // The single most safety-critical path in this driver (arbitrary
    // host-shell execution, gated on the allow-list) had zero test coverage
    // before this — see `feature-quality`/`hermeticity` review findings.

    fn write_package_json_with_scripts(dir: &StdPath, scripts_json: &str) {
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name": "pkg", "scripts": {scripts_json}}}"#),
        )
        .expect("write package.json fixture");
    }

    #[test]
    fn run_lifecycle_scripts_no_package_json_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_lifecycle_scripts(dir.path(), "pkg")
            .expect("no package.json at all must be a no-op, not an error");
    }

    #[test]
    fn run_lifecycle_scripts_no_scripts_key_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("package.json"), r#"{"name": "pkg"}"#)
            .expect("write package.json fixture");
        run_lifecycle_scripts(dir.path(), "pkg")
            .expect("a package.json with no `scripts` key must be a no-op");
    }

    #[test]
    fn run_lifecycle_scripts_runs_postinstall_and_its_side_effect_is_observable() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_package_json_with_scripts(dir.path(), r#"{"postinstall": "echo done > marker.txt"}"#);
        run_lifecycle_scripts(dir.path(), "pkg").expect("allow-listed postinstall must run");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("marker.txt"))
                .expect("read marker")
                .trim(),
            "done"
        );
    }

    #[test]
    fn run_lifecycle_scripts_runs_preinstall_install_postinstall_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_package_json_with_scripts(
            dir.path(),
            r#"{
                "preinstall": "echo pre >> order.txt",
                "install": "echo install >> order.txt",
                "postinstall": "echo post >> order.txt"
            }"#,
        );
        run_lifecycle_scripts(dir.path(), "pkg").expect("all three scripts must run");
        let order = std::fs::read_to_string(dir.path().join("order.txt")).expect("read order.txt");
        let lines: Vec<&str> = order.lines().collect();
        assert_eq!(lines, vec!["pre", "install", "post"]);
    }

    /// The stdio-inheritance bug this fixes: a script's own output must be
    /// *captured*, not inherited straight through to whatever heph's own
    /// stdout/stderr happen to be (the real terminal, which the TUI owns
    /// via the alternate screen — confirmed live, a script writing there
    /// corrupted it). Captured output is unobservable from a unit test
    /// process's own stdio, but a *failing* script's captured stdout/stderr
    /// landing in the returned error is directly observable, and proves
    /// the same capture mechanism.
    #[test]
    fn run_lifecycle_scripts_captures_script_output_into_the_failure_not_the_terminal() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_package_json_with_scripts(
            dir.path(),
            r#"{"postinstall": "echo building-native-thing; echo compiler-error >&2; exit 1"}"#,
        );
        let err = run_lifecycle_scripts(dir.path(), "native-thing")
            .expect_err("a non-zero exit must still fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("building-native-thing"),
            "the script's own stdout must be captured into the error: {msg}"
        );
        assert!(
            msg.contains("compiler-error"),
            "the script's own stderr must be captured into the error: {msg}"
        );
    }

    #[test]
    fn run_lifecycle_scripts_nonzero_exit_fails_naming_the_script_and_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_package_json_with_scripts(dir.path(), r#"{"postinstall": "exit 7"}"#);
        let err = run_lifecycle_scripts(dir.path(), "native-thing")
            .expect_err("a non-zero script exit must fail, not silently succeed");
        let msg = format!("{err:#}");
        assert!(msg.contains("postinstall"), "must name the script: {msg}");
        assert!(msg.contains("native-thing"), "must name the package: {msg}");
    }

    #[test]
    fn run_lifecycle_scripts_nonzero_exit_stops_remaining_scripts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_package_json_with_scripts(
            dir.path(),
            r#"{
                "preinstall": "exit 3",
                "postinstall": "echo should-not-run >> marker.txt"
            }"#,
        );
        run_lifecycle_scripts(dir.path(), "pkg")
            .expect_err("preinstall failing must fail the whole call");
        assert!(
            !dir.path().join("marker.txt").exists(),
            "postinstall must never run after preinstall already failed"
        );
    }
}
