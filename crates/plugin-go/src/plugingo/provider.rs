use crate::plugingo::addr_util::{
    GoPackageKind, decode_package, encode_firstparty, encode_stdlib, encode_thirdparty,
    encode_thirdparty_download,
};
use crate::plugingo::errors::NoGoFilesError;
use crate::plugingo::factors::{Factors, VariantRef, current_goarch, current_goos};
use crate::plugingo::govet;
use crate::plugingo::pkg_analysis::{
    GoPackage, PackageAddrs, decode_go_package, decode_package_addrs, find_module_for_import,
    is_stdlib_import_path, parse_go_mod_module_path, parse_go_mod_requires, parse_go_sum_modules,
};
use crate::plugingo::target_bin;
use crate::plugingo::target_golist;
use crate::plugingo::target_lib;
use crate::plugingo::target_modfiles;
use crate::plugingo::target_std;
use crate::plugingo::target_test;
use crate::plugingo::thirdparty;
use crate::plugingo::toolchain;
use crate::plugingo::variant;
use anyhow::Context;
use async_recursion::async_recursion;
use async_trait::async_trait;
use enclose::enclose;
use futures::future::{BoxFuture, try_join_all};
use hbuiltins::pluginfs;
use hcore::hasync::Cancellable;
use hcore::hmemoizer::{Memoizer, downcast_chain_ref, unwrap_arc_err};
use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use hcore::htvalue::{Value, parse_map_string_strings, parse_strings};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, FnArgs, FnCallContext, GetError, GetRequest, GetResponse,
    ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse, Provider as ProviderTrait,
    ProviderExecutor, ProviderFn, ProviderFunctionDef, State,
};
use hwalk::{CachedWalker, EntryKind, Ignore};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Config {
    /// Toolchain spec every build/test/list target resolves against. Either a
    /// pinned version (hermetic SDK at `//@heph/go/toolchain/<go_version>:go`)
    /// or [`toolchain::HOST`] (use the host `go`). Set via the required `gotool`
    /// provider option; programmatic callers (tests) set it here directly.
    pub go_version: String,
    /// Expected SHA-256 of each hermetic SDK tarball, keyed by
    /// [`toolchain::checksum_key`] (`"<version>/<goos>/<goarch>"`). Populated
    /// from the optional `checksums` provider option. Optional: a host triple
    /// absent here downloads unverified (the toolchain driver warns); supply an
    /// entry to enforce verification. Empty for `gotool = "host"`.
    pub sdk_checksums: HashMap<String, String>,
    /// Addr of the `heph-govet` binary the lint/format targets exec. Defaults to
    /// [`govet::default_addr`] — the `http_fetch` target that downloads the
    /// binary published in this plugin's own heph release. Point it at a build
    /// target (e.g. `//tools/heph-govet:build`, inside heph's own repo) to use a
    /// from-source binary instead. Set via the optional `govet` provider option.
    ///
    /// The addr is taken verbatim if it carries args; otherwise the host's go
    /// factors (`goos`/`goarch`) are added, since the tool always runs natively
    /// (see [`ProviderInner::govet_tool_addr`]).
    pub govet: String,
    /// Directories pruned during package discovery: engine skip dirs/globs plus
    /// this provider's own `skip` option. See [`hwalk::Ignore`].
    pub skip: Arc<Ignore>,
    /// Reject target names this provider doesn't own *before* the `_golist`
    /// resolve (see [`is_known_go_target_name`]). On by default: it avoids a
    /// wasted `go list` for foreign names (e.g. a buildfile codegen target
    /// sharing a Go package dir) and the cycle that resolve would induce. The
    /// engine contains that cycle regardless (a cyclic provider attempt falls
    /// through to the next provider), so this is a perf/clarity guard, not a
    /// correctness crutch — tests that exercise the engine's containment path
    /// turn it off.
    pub foreign_name_guard: bool,
    /// Shared cross-run filesystem-walk cache. `collect_go_packages` reads each
    /// directory through it, so an unchanged tree skips `readdir` (package
    /// identity depends only on the directory layout, which the walker caches by
    /// mtime). Disabled by default — unit tests that build a bare provider walk
    /// live; the engine injects the real shared walker via `from_options`.
    pub walker: Arc<CachedWalker>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            go_version: toolchain::DEFAULT_GO_VERSION.to_string(),
            sdk_checksums: HashMap::new(),
            govet: govet::default_addr(),
            skip: Arc::new(Ignore::default()),
            foreign_name_guard: true,
            walker: Arc::new(CachedWalker::disabled()),
        }
    }
}

/// Public façade: thin wrapper around `Arc<ProviderInner>` so trait-method
/// closures (e.g. `Memoizer::once` in `collect_*_libs`) can capture `Arc<Self>`
/// without dragging `&self` lifetimes through `'static` future bounds.
pub struct Provider {
    inner: Arc<ProviderInner>,
}

pub(crate) struct ProviderInner {
    workspace_root: PathBuf,
    /// Go release the hermetic toolchain is pinned to (see [`Config::go_version`]).
    go_version: String,
    /// Expected SDK tarball checksums by [`toolchain::checksum_key`] (see
    /// [`Config::sdk_checksums`]). Also carries the optional `heph-govet` binary
    /// checksums, under [`govet::checksum_key`]'s `govet/…` namespace.
    sdk_checksums: HashMap<String, String>,
    /// Addr of the `heph-govet` binary lint/format exec (see [`Config::govet`]).
    govet: String,
    /// Directories pruned during `collect_go_packages` (engine home + user globs).
    skip: Arc<Ignore>,
    /// Shared cross-run fs-walk cache backing the package walk. See [`Config::walker`].
    walker: Arc<CachedWalker>,
    /// See [`Config::foreign_name_guard`].
    foreign_name_guard: bool,
    /// Cache: golist addr → parsed GoPackage. Memoizes the artifact parse only;
    /// the underlying `executor.result(golist_addr)` is always called outside
    /// the `once` closure so every caller (owner + waiter) registers the
    /// `parent → golist_addr` edge in the engine's `DepDag`. Caching the
    /// executor call here would let a waiter skip dep registration and hide a
    /// target-dep cycle as a memoizer deadlock.
    pkg_cache: Memoizer<Addr, Result<Arc<GoPackage>, Arc<anyhow::Error>>>,
    /// Cache: golist addr → driver-resolved per-file addresses. Same constraint
    /// as `pkg_cache` — caches the parse, not the executor call.
    pkg_addrs_cache: Memoizer<Addr, Result<Arc<PackageAddrs>, Arc<anyhow::Error>>>,
    /// Cache: dedup `collect_direct_libs` / `collect_transitive_libs` BFS across
    /// `handle_get` calls (`build_lib`, `build`, `build_test`, `build_test_lib`,
    /// `build_xtest_lib`, `build_testmain_lib`) that share the same root pkg + factors.
    libs_cache: Memoizer<LibsKey, Result<Arc<TransitiveDeps>, Arc<anyhow::Error>>>,
    /// Cache: per-`(import_path, factors, module_root)` transitive closure. Used by the
    /// `transitive=true` path of `collect_libs_inner`: each top-level walk recursively
    /// composes per-import sub-closures from this cache instead of BFS-walking the
    /// full subtree itself. Subtree-sharing across all consumers — e.g. if 200
    /// top-level targets all transitively depend on `fmt`, `fmt`'s closure is computed
    /// once total, not 200 times.
    import_closure_cache:
        Memoizer<ImportClosureKey, Result<Arc<ImportClosure>, Arc<anyhow::Error>>>,
    /// Cache: parsed `go.mod` per `module_root`. `collect_libs_inner` is invoked
    /// per `LibsKey` (≥ K×N for K target variants × N root pkgs in module), so
    /// the same `go.mod` is otherwise read+parsed hundreds of times per build.
    /// Race-tolerant; parse is sync, idempotent, µs-scale — no Memoizer needed.
    go_mod_cache: RwLock<HashMap<PathBuf, Arc<GoModData>>>,
}

#[derive(Debug)]
pub(crate) struct GoModData {
    pub requires: Vec<(String, String)>,
    pub module_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LibsKey {
    imports: Vec<String>,
    extra: Vec<String>,
    vref: VariantRef,
    module_root: PathBuf,
    transitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImportClosureKey {
    import_path: String,
    vref: VariantRef,
    module_root: PathBuf,
}

/// Result of `import_closure(ip)`: the import_path's lib (if any) plus its full
/// transitive closure, in topological order (deps before dependents). Composing
/// multiple `ImportClosure`s requires de-duplication by `import_path` at the
/// composition site (see `compose_closures`).
///
/// Import paths are `Arc<str>` so that composing a node's closure from its
/// children's closures (which copies every transitive entry, at every node — an
/// O(closure-size) copy per node) is a refcount bump rather than a String
/// heap-allocation+copy. The flattened result is converted to owned `String`s
/// once, at the top of `collect_libs_inner`.
#[derive(Debug, Clone, Default)]
struct ImportClosure {
    libs: Vec<(Arc<str>, Addr)>,
}

/// Flatten already-deps-first-deduped `sub_closures` into one deps-first list,
/// deduped by import_path, with `self_lib` (if any) appended last. Shared by
/// `import_closure` (composing a node from its children's closures) and
/// `collect_libs_inner` (composing the transitive set from the root imports).
///
/// Import paths are `Arc<str>`, so re-listing a child's transitive entries here —
/// which happens at *every* node, an O(closure-size) copy per node — is a refcount
/// bump rather than a String heap-copy. The dedup set and result are pre-sized to
/// the summed child closure size (+ self).
fn compose_closures(
    sub_closures: &[Arc<ImportClosure>],
    self_lib: Option<(Arc<str>, Addr)>,
) -> Vec<(Arc<str>, Addr)> {
    let extra = self_lib.is_some() as usize;
    let cap = sub_closures.iter().map(|s| s.libs.len()).sum::<usize>() + extra;
    let mut seen: HashSet<Arc<str>> = HashSet::with_capacity(cap);
    let mut libs: Vec<(Arc<str>, Addr)> = Vec::with_capacity(cap);
    for sub in sub_closures {
        for (ip, addr) in &sub.libs {
            if seen.insert(Arc::clone(ip)) {
                libs.push((Arc::clone(ip), addr.clone()));
            }
        }
    }
    if let Some((ip, addr)) = self_lib
        && seen.insert(Arc::clone(&ip))
    {
        libs.push((ip, addr));
    }
    libs
}

impl Provider {
    pub fn new(workspace_root: PathBuf) -> anyhow::Result<Self> {
        Self::with_config(workspace_root, Config::default())
    }

    pub fn from_options(
        workspace_root: PathBuf,
        skip_dirs: &[PathBuf],
        skip_globs: &[String],
        opts: &hplugin::config::Options,
        walker: Arc<CachedWalker>,
    ) -> anyhow::Result<Self> {
        // `gotool` selects the Go toolchain and is REQUIRED — there is no
        // implicit default. Set it to:
        //   - `"host"` → use the host `go` (read from PATH / `go env GOROOT`
        //     inside the sandbox; non-hermetic, see [`toolchain::HOST`]),
        //   - a pinned version like `"1.26.4"` → download + manage that SDK
        //     hermetically (`//@heph/go/toolchain/<version>:go`), or
        //   - a target address like `"//@heph/bin:go"` (host `go` via the hostbin
        //     provider) or `"//some/pkg:go"` (e.g. a nix-built `go`) → use the
        //     `go` produced by that target (see [`toolchain::is_target_ref`]).
        hplugin::config::deny_unknown(
            "go provider",
            opts,
            &["gotool", "govet", "skip", "checksums"],
        )?;
        let go_version: String = hplugin::config::decode_opt(opts, "go provider", "gotool")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "go provider: `gotool` is required — set it to \"host\" (use the host go), \
                     a pinned version like \"{}\" (hermetic SDK download), or a target address \
                     like \"//@heph/bin:go\" (a target that produces the go toolchain)",
                    toolchain::DEFAULT_GO_VERSION
                )
            })?;
        // Optional: SDK tarball checksums keyed `"<version>/<goos>/<goarch>"`
        // (see `toolchain::checksum_key`). Required in practice only for a
        // hermetic `gotool` — resolved lazily when the toolchain target is built,
        // so `gotool = "host"` needs none.
        let sdk_checksums: HashMap<String, String> =
            hplugin::config::decode_opt(opts, "go provider", "checksums")?.unwrap_or_default();
        // Optional: the addr of the `heph-govet` binary lint/format exec. Unset →
        // the `http_fetch` target that downloads the binary published in this
        // plugin's own release (see `govet::default_addr`). Point it at a build
        // target (`//tools/heph-govet:build`) to run one built from source.
        let govet: String = hplugin::config::decode_opt(opts, "go provider", "govet")?
            .unwrap_or_else(govet::default_addr);
        // Engine-wide `fs.skip` globs are merged ahead of this provider's own
        // `skip` option so both prune the same workspace-relative paths.
        let mut globs = skip_globs.to_vec();
        let user_skip: Vec<String> =
            hplugin::config::decode_opt(opts, "go provider", "skip")?.unwrap_or_default();
        globs.extend(user_skip);
        let skip = Arc::new(Ignore::new(skip_dirs, &globs)?);
        Self::with_config(
            workspace_root,
            Config {
                go_version,
                sdk_checksums,
                govet,
                skip,
                walker,
                ..Default::default()
            },
        )
    }

    pub fn with_config(workspace_root: PathBuf, config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Arc::new(ProviderInner {
                workspace_root,
                go_version: config.go_version,
                sdk_checksums: config.sdk_checksums,
                govet: config.govet,
                skip: config.skip,
                walker: config.walker,
                foreign_name_guard: config.foreign_name_guard,
                pkg_cache: Memoizer::with_tag("pkg_cache"),
                pkg_addrs_cache: Memoizer::with_tag("pkg_addrs_cache"),
                libs_cache: Memoizer::with_tag("libs_cache"),
                import_closure_cache: Memoizer::with_tag("import_closure_cache"),
                go_mod_cache: RwLock::new(HashMap::new()),
            }),
        })
    }
}

/// Recursively enumerate directories at or below `dir` that live under a
/// `go.mod` ancestor. Replaces the prior `.go`-file-based detection: any dir
/// under a Go module is a candidate package, and `_golist` decides what is real
/// when the engine actually queries it.
///
/// `under_gomod` is the inherited flag from the parent walk — once we've found
/// a `go.mod` at or above the current dir, every descendant inherits it and we
/// skip the per-dir `find_go_mod` lookup. The `find_go_mod` cache absorbs the
/// cost when the flag is unset.
fn collect_go_packages(
    walker: &CachedWalker,
    dir: &Path,
    workspace_root: &Path,
    under_gomod: bool,
    skip: &Ignore,
    result: &mut Vec<anyhow::Result<ListPackageResponse>>,
) {
    let is_under = under_gomod || crate::plugingo::addr_util::find_go_mod(dir).is_some();

    if is_under {
        let rel = dir.strip_prefix(workspace_root).unwrap_or(dir);
        // A package identifier, so `to_str` rather than a lossy render that could
        // fold two distinct directories onto one `\u{FFFD}` name — see the same
        // check in the buildfile provider. Unreachable while every walked
        // component comes from `CachedWalker::read_dir`, which rejects non-UTF-8
        // names; kept so the invariant is asserted rather than assumed.
        match rel.to_str() {
            Some(pkg) => result.push(Ok(ListPackageResponse {
                pkg: PkgBuf::from(pkg),
            })),
            // Blames the workspace root, not the package directory: every
            // component below the root comes from the walker, which rejects
            // non-UTF-8 names, so the root is the only thing left that can make
            // this relative path undecodable.
            None => {
                result.push(Err(anyhow::anyhow!(
                    "package path is not valid UTF-8: '{}' (under workspace root '{}')",
                    dir.display(),
                    workspace_root.display()
                )));
                // Same as the `read_dir` arm below: stop here rather than emit
                // one copy of the same error for every descendant.
                return;
            }
        }
    }

    // Read through the shared walker: an unchanged tree skips the `readdir`
    // syscall entirely (the cached listing is keyed by directory mtime). A
    // symlinked dir lists as `Symlink`, not `Dir`, so it is not descended —
    // matching the previous `file_type()` (no-follow) behavior.
    let listing = match walker.read_dir(dir) {
        Ok(l) => l,
        Err(e) => {
            result.push(Err(e.context(format!("read_dir {}", dir.display()))));
            return;
        }
    };

    for entry in &listing.entries {
        if entry.kind != EntryKind::Dir {
            continue;
        }
        // Skip dot/underscore-prefixed dirs and the go-convention non-package
        // dirs by raw bytes. A leading `.`/`_` is a single ASCII byte and UTF-8
        // lead/continuation bytes are all >= 0x80, so a byte compare can't
        // misfire on a multibyte name.
        let bytes = entry.name.as_bytes();
        if matches!(bytes.first(), Some(b'.' | b'_')) || bytes == b"vendor" || bytes == b"testdata"
        {
            continue;
        }
        let entry_path = dir.join(&entry.name);
        let rel = entry_path
            .strip_prefix(workspace_root)
            .unwrap_or(&entry_path);
        if skip.prune_dir(&entry_path, rel) {
            continue;
        }
        collect_go_packages(walker, &entry_path, workspace_root, is_under, skip, result);
    }
}

impl ProviderTrait for Provider {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        self.inner.config(req)
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        self.inner.list(req, ctoken)
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        self.inner.list_packages(req, ctoken)
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.handle_get(req).await })
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        self.inner.probe(req, ctoken)
    }

    fn functions(&self) -> Vec<ProviderFunctionDef> {
        vec![ProviderFunctionDef {
            name: "build_addr".to_string(),
            signature: FnSignature {
                positional: vec![
                    Param::required("pkg", ParamType::String),
                    Param::optional("variant", ParamType::String, Value::String(String::new())),
                ],
                named: vec![],
                variadic: None,
                returns: ParamType::String,
            },
            doc: "Build the address of a Go package's user-facing `build` target, as \
                  used in `deps`. With a variant name, returns \
                  `//<pkg>:build@v=<variant>`. Omit `variant` (or pass \"\") to get \
                  the magic host-default target `//<pkg>:build` — a `group` that \
                  forwards to the first variant matching this machine's os/arch. The \
                  provider resolves the variant (and pins the defining package) when \
                  built."
                .to_string(),
            func: Arc::new(BuildAddrFn),
        }]
    }

    fn state_schema(&self) -> Option<hplugin::provider::StateSchema> {
        use hplugin::provider::{StateField, StateSchema};
        let field = |name: &str, ty: ParamType, doc: &str| StateField {
            name: name.to_string(),
            ty,
            doc: doc.to_string(),
            required: false,
        };
        let variant_struct = ParamType::strukt(vec![
            ("goos", ParamType::String),
            ("goarch", ParamType::String),
            ("tags", ParamType::list(ParamType::String)),
            ("goexperiment", ParamType::list(ParamType::String)),
            ("gcflags", ParamType::list(ParamType::String)),
            ("ldflags", ParamType::list(ParamType::String)),
        ]);
        Some(StateSchema {
            fields: vec![
                field(
                    "variants",
                    ParamType::map(variant_struct),
                    "Named Go build variants for this package (and its descendants, via \
                     closest-ancestor lookup). Maps a variant name to a static factor set: \
                     `goos`/`goarch` (required), plus optional `tags` (build tags), \
                     `goexperiment` (GOEXPERIMENT), `gcflags` (extra `go tool compile` flags) \
                     and `ldflags` (extra `go tool link` flags). \
                     A user-facing target selects one with `@v=NAME`, resolving the closest \
                     ancestor package that defines that name; variants do NOT compound across \
                     the tree (each definition is self-contained).",
                ),
                field(
                    "go_codegen_root",
                    ParamType::Bool,
                    "Mark this package (and descendants) as a Go codegen root. Always applies \
                     to descendants, independent of `recursive`.",
                ),
                field(
                    "go_codegen_deps",
                    ParamType::list(ParamType::String),
                    "Extra dependencies (target addresses) injected into generated Go targets. \
                     Always applies to descendants, independent of `recursive`.",
                ),
                field(
                    "test",
                    ParamType::union(vec![
                        ParamType::Bool,
                        ParamType::strukt(vec![
                            ("env", ParamType::map(ParamType::String)),
                            ("pass_env", ParamType::list(ParamType::String)),
                            ("runtime_env", ParamType::map(ParamType::String)),
                            ("runtime_pass_env", ParamType::list(ParamType::String)),
                            ("pre_run", ParamType::list(ParamType::String)),
                        ]),
                    ]),
                    "Test settings for this package. `test = False` skips its tests, \
                     `test = True` (or unset) runs them. \
                     The struct form configures the generated `test`/`xtest` run \
                     targets — `env`/`runtime_env` (map[string]) and \
                     `pass_env`/`runtime_pass_env` (list[string]) set env, and \
                     `pre_run` (list[string]) runs shell lines before the test binary \
                     (switching the target to the bash driver). By default applies \
                     only to this package; set `recursive = True` to apply to \
                     descendant packages too.",
                ),
                field(
                    "link",
                    ParamType::strukt(vec![
                        ("flags", ParamType::list(ParamType::String)),
                        ("deps", link_deps_param_type()),
                        ("runtime_deps", link_deps_param_type()),
                    ]),
                    "Link settings for a `main` package's `build` (binary) target. \
                     `flags` are passed verbatim to `go tool link`; `deps` are target \
                     addresses staged into the link sandbox (hashed inputs) that the \
                     flags can reference; `runtime_deps` travel with the binary at run \
                     time only (not hashed). `deps`/`runtime_deps` accept a list of \
                     addresses or a `{group: [addr, …]}` map to name dep groups. By \
                     default applies only to this package; set `recursive = True` to \
                     apply to descendant packages too.",
                ),
                field(
                    "recursive",
                    ParamType::Bool,
                    "Apply this state's config (the `test` toggle/struct and `link = {...}`) \
                     to descendant packages, not just the exact declaring package. \
                     `go_codegen_root`/`go_codegen_deps` are unaffected — they always \
                     apply to descendants.",
                ),
            ],
        })
    }
}

/// `heph.go.build_addr(pkg, v)` — format the heph address of a Go package's
/// user-facing `build` target for variant `v`, without resolving anything. Takes
/// a heph package (the addr's package, e.g. `"mylib"`, `"@heph/go/std/fmt"`, or a
/// thirdparty `@heph/go/thirdparty/…@v` path) and a variant name, and returns the
/// canonical addr string `//<pkg>:build@v=<variant>`. Pure string transform — the
/// provider resolves the variant (and pins its defining package) at get time.
struct BuildAddrFn;

impl BuildAddrFn {
    fn arg_str<'a>(args: &'a FnArgs, idx: usize, name: &str) -> anyhow::Result<&'a str> {
        let v = args
            .named
            .get(name)
            .or_else(|| args.positional.get(idx))
            .ok_or_else(|| anyhow::anyhow!("heph.go.build_addr: missing `{name}` argument"))?;
        match v {
            Value::String(s) => Ok(s.as_str()),
            other => anyhow::bail!("heph.go.build_addr: `{name}` must be a string, got {other:?}"),
        }
    }

    /// Optional string arg: `None` when absent, error when present but non-string.
    fn opt_arg_str<'a>(
        args: &'a FnArgs,
        idx: usize,
        name: &str,
    ) -> anyhow::Result<Option<&'a str>> {
        match args.named.get(name).or_else(|| args.positional.get(idx)) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(other) => {
                anyhow::bail!("heph.go.build_addr: `{name}` must be a string, got {other:?}")
            }
        }
    }
}

#[async_trait]
impl ProviderFn for BuildAddrFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        let pkg = Self::arg_str(&args, 0, "pkg")?;
        let v = Self::opt_arg_str(&args, 1, "variant")?.unwrap_or("");

        // With a variant name, a user-facing `build` target carries only `v` (the
        // provider resolves the closest variant and fills in `vp` when built).
        // Without one, return the magic host-default `//<pkg>:build` — a bare,
        // variant-less addr the provider serves as a `group` forwarding to the
        // first variant matching this machine's os/arch.
        let addr_args = if v.is_empty() {
            BTreeMap::new()
        } else {
            BTreeMap::from([("v".to_string(), v.to_string())])
        };
        let addr = Addr::new(PkgBuf::from(pkg), "build".to_string(), addr_args);
        Ok(Value::String(addr.format()))
    }
}

impl ProviderInner {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "go".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            let empty = || {
                Ok(Box::new(std::iter::empty())
                    as Box<
                        dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                    >)
            };

            let kind = match decode_package(&req.package, &self.workspace_root) {
                Some(k) => k,
                None => return empty(),
            };

            // A first-party package inside a skipped subtree lists nothing —
            // mirroring what the package walk (`collect_go_packages`) prunes.
            if matches!(&*kind, GoPackageKind::FirstParty { .. })
                && self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.package.as_str()))
            {
                return empty();
            }

            // Every build/list target is variant-parameterized. Two enumeration
            // scopes:
            //   - **entry / binary** targets (`build`, `test`, `lint`, …) list the
            //     module-bounded *ancestry* variants — the ones a user can select
            //     at this package.
            //   - **library / intermediate** targets (`build_lib`, `_golist`, …)
            //     list the whole module *universe* (fetched via `states_under`), so
            //     a variant declared at a sibling package is still enumerated — the
            //     forms a cross-subtree consumer pins with `vp`.
            // A package with no variants in scope lists no build targets — there is
            // no implicit default variant.
            let module_root = module_root_rel(&kind, &self.workspace_root);
            let ancestry_pairs = variant::ancestry_variants_with_factors(&req.states, &module_root);
            let ancestry: Vec<VariantRef> = ancestry_pairs.iter().map(|(v, _)| v.clone()).collect();
            // Runnable `test`/`xtest` targets execute the built binary, so they
            // only make sense for the host platform — enumerate them for the
            // ancestry variants whose `goos`/`goarch` match this machine. The
            // corresponding `build_test`/`build_xtest` (cross-compilable) still
            // list for every variant, below.
            let (host_goos, host_goarch) = (current_goos(), current_goarch());
            let ancestry_host: Vec<VariantRef> = ancestry_pairs
                .iter()
                .filter(|(_, f)| f.goos == host_goos && f.goarch == host_goarch)
                .map(|(v, _)| v.clone())
                .collect();
            // First-party libs enumerate the module universe; stdlib/thirdparty
            // (rarely listed directly, and always consumed with a `vp` pin) fall
            // back to ancestry.
            let universe = if matches!(&*kind, GoPackageKind::FirstParty { .. }) {
                // `states_under` walks the whole subtree by path prefix, so it
                // also returns states from *nested* submodules under this
                // package. Keep only states in this target's own module — a
                // nested submodule's variants are a different module's targets
                // and must not be enumerated here.
                let module_states: Vec<State> = req
                    .executor
                    .states_under(&hmodel::htpkg::PkgBuf::from(module_root.as_str()))
                    .await?
                    .into_iter()
                    .filter(|s| {
                        pkg_in_module(s.package.as_str(), &module_root, &self.workspace_root)
                    })
                    .collect();
                variant::universe_variants(&module_states)?
            } else {
                ancestry.clone()
            };
            let vrefs_for = |name: &str| -> &[VariantRef] {
                if is_run_test_target_name(name) {
                    &ancestry_host
                } else if is_entry_target_name(name) {
                    &ancestry
                } else {
                    &universe
                }
            };

            let push_names = |addrs: &mut Vec<Addr>, names: &[&str]| {
                for name in names {
                    for vref in vrefs_for(name) {
                        addrs.push(Addr::new(
                            req.package.clone(),
                            (*name).to_string(),
                            vref.to_args(),
                        ));
                    }
                }
            };

            let mut addrs: Vec<Addr> = Vec::new();
            match &*kind {
                GoPackageKind::Stdlib { .. } => {
                    push_names(&mut addrs, &["_golist", "build_lib"]);
                }
                GoPackageKind::ThirdParty { subpath, .. } => {
                    push_names(&mut addrs, &["_golist", "build_lib"]);
                    // The `download` target lives at the module root only and is
                    // variant-independent (one per module@version).
                    if subpath.is_empty() {
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "download".to_string(),
                            Default::default(),
                        ));
                    }
                }
                GoPackageKind::FirstParty { module_root, .. } => {
                    // Lint/format targets exist only for modules that opt in with a
                    // golangci config at the go.mod root, so gate them here
                    // (matching the `get`-time gate) to avoid advertising targets
                    // that would resolve to NotFound.
                    let lint_enabled = self.golangci_config_addr(module_root).is_some();
                    let skip_tests = pick_test_skip(&req.states, req.package.as_str());
                    push_names(&mut addrs, &["_golist", "build_lib", "build", "embed"]);
                    // Magic host-default `build` (bare, no `@v`): a `group`
                    // forwarding to the first host-matching variant. Listed only
                    // when such a variant exists in ancestry.
                    if !ancestry_host.is_empty() {
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "build".to_string(),
                            Default::default(),
                        ));
                    }
                    if lint_enabled {
                        // One bare addr per package for both families, never
                        // multiplied across variants: formatting is syntactic and
                        // so variant-free outright (`VARIANT_FREE_TARGET_NAMES`),
                        // while the lint gate/fixer aggregate every ancestry
                        // variant's analysis (`VARIANT_AGGREGATE_TARGET_NAMES`).
                        // The per-variant `_lint-analyze` stays unlisted (as
                        // before): it is internal, and the aggregators pull it in
                        // as a dep.
                        for name in VARIANT_AGGREGATE_TARGET_NAMES
                            .iter()
                            .chain(VARIANT_FREE_TARGET_NAMES)
                        {
                            addrs.push(Addr::new(
                                req.package.clone(),
                                (*name).to_string(),
                                Default::default(),
                            ));
                        }
                    }
                    if !skip_tests {
                        push_names(
                            &mut addrs,
                            &["embed_xtest", "build_test", "test", "build_xtest", "xtest"],
                        );
                    }
                }
            }

            let responses: Vec<anyhow::Result<ListResponse>> = addrs
                .into_iter()
                .map(|addr| Ok(ListResponse { addr }))
                .collect();
            Ok(Box::new(responses.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let prefix = req.prefix.as_str();

            // Can't enumerate stdlib or thirdparty packages
            if prefix.starts_with("@heph/go/") {
                return Ok(Box::new(std::iter::empty())
                    as Box<
                        dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                    >);
            }

            let search_dir = if prefix.is_empty() {
                self.workspace_root.clone()
            } else {
                self.workspace_root.join(prefix)
            };

            if !search_dir.exists() {
                return Ok(Box::new(std::iter::empty())
                    as Box<
                        dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                    >);
            }

            let packages = hcore::blocking::run(
                enclose!((self.workspace_root => workspace_root, self.skip => skip, self.walker => walker) move || {
                    let mut packages = Vec::new();
                    collect_go_packages(&walker, &search_dir, &workspace_root, false, &skip, &mut packages);
                    packages
                }),
            )
            .await;

            Ok(Box::new(packages.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}

#[derive(Clone)]
struct TransitiveDeps {
    /// `(import_path, build_lib_addr)` for every reachable dep (for importcfg).
    libs: Vec<(String, Addr)>,
}

/// Pick the closest (deepest) ancestor `provider_state(provider="go", ...)` that
/// has `go_codegen_root=True` and whose package is a prefix of `addr_pkg`.
///
/// Engine pre-filters states by provider name, so callers only see Go states.
fn pick_codegen_root(states: &[State]) -> Option<&State> {
    states
        .iter()
        .filter(|s| matches!(s.state.get("go_codegen_root"), Some(Value::Bool(true))))
        .max_by_key(|s| s.package.as_str().len())
}

/// Go test-target name set. Every variant the provider may emit (list) or
/// resolve (get) that exists solely to support `go test`. Used by
/// `pick_test_skip` to gate both endpoints from a single source of truth.
const TEST_TARGET_NAMES: &[&str] = &[
    "test",
    "xtest",
    "build_test",
    "build_xtest",
    "build_test_lib",
    "build_xtest_lib",
    "build_testmain_lib",
    "build_xtestmain_lib",
    "testmain",
    "xtestmain",
    "embed_test",
    "embed_xtest",
];

fn is_test_target_name(name: &str) -> bool {
    TEST_TARGET_NAMES.contains(&name)
}

/// User-facing **entry / binary** target names — the ones a user selects a
/// variant on directly (bare `@v`, resolved by module-bounded ancestry). Every
/// other target is a **library / intermediate** (carries `vp`, resolved against
/// the module universe). Used by `list` to choose the enumeration scope
/// (ancestry vs universe) per target name.
///
/// `format`/`format-check` are deliberately absent: formatting is syntactic, so
/// they are variant-independent (see [`VARIANT_FREE_TARGET_NAMES`]). So are
/// `lint-check`/`lint`, which aggregate over every ancestry variant instead of
/// being selected with one (see [`VARIANT_AGGREGATE_TARGET_NAMES`]).
fn is_entry_target_name(name: &str) -> bool {
    matches!(name, "build" | "test" | "xtest")
}

/// **Runnable** test target names — the ones that execute the built test binary
/// (as opposed to `build_test`/`build_xtest`, which only cross-compile it). Only
/// these are gated to the host `goos`/`goarch` in `list`, since a test binary
/// built for another platform can't run here.
fn is_run_test_target_name(name: &str) -> bool {
    matches!(name, "test" | "xtest")
}

/// Workspace-relative go.mod directory of a decoded package (`""` for a root
/// module, `"go"` for `go/go.mod`, etc.). Two packages belong to the *same* Go
/// module iff this matches — the real module-membership test (a plain path
/// prefix is wrong: a package can prefix-match a module root while living inside
/// a *nested* submodule with its own `go.mod`).
fn module_root_rel(kind: &GoPackageKind, workspace_root: &Path) -> String {
    match kind {
        GoPackageKind::FirstParty { module_root, .. }
        | GoPackageKind::ThirdParty { module_root, .. } => module_root
            .strip_prefix(workspace_root)
            .unwrap_or(module_root)
            .to_string_lossy()
            .into_owned(),
        GoPackageKind::Stdlib { .. } => String::new(),
    }
}

/// Whether `pkg` (a Go import package) belongs to the module rooted at
/// `target_module_root` (workspace-relative). The real nearest-`go.mod` check —
/// used to bound `vp` honoring and library-variant enumeration to the target's
/// own module, so a nested submodule's variants never leak across the boundary.
fn pkg_in_module(pkg: &str, target_module_root: &str, workspace_root: &Path) -> bool {
    match decode_package(&hmodel::htpkg::PkgBuf::from(pkg), workspace_root) {
        Some(kind) => module_root_rel(&kind, workspace_root) == target_module_root,
        None => false,
    }
}

/// Spec for the magic host-default `build` (bare `//pkg:build`): a `group` target
/// that re-exports the outputs of the concrete `build@v=<variant>` it forwards to.
fn magic_build_group_spec(addr: Addr, target: &Addr) -> hplugin::provider::TargetSpec {
    let config = HashMap::from([(
        "deps".to_string(),
        Value::List(vec![Value::String(target.format())]),
    )]);
    hplugin::provider::TargetSpec {
        addr,
        driver: hbuiltins::plugingroup::DRIVER_NAME.to_string(),
        config,
        ..Default::default()
    }
}

/// Targets handled by `handle_get` *before* the `_golist` resolve (no go list
/// needed): the golist target itself, the go.mod copy, and the module download.
const SPECIAL_TARGET_NAMES: &[&str] = &["_golist", "_go_mod", "download"];

/// Non-test first-party/thirdparty target names this provider owns and resolves
/// through `_golist` (see the `match addr.name` arms in `handle_get`).
const GOLIST_TARGET_NAMES: &[&str] = &["build_lib", "build", "embed", "_lint-analyze"];

/// Target names this provider owns that are **variant-free**: they carry no `@v`
/// / `@vp` and are handled before variant resolution.
///
/// Formatting is purely syntactic — gofmt/gofumpt/goimports never look at
/// `GOOS`/`GOARCH`/build tags — so a per-variant `format` would be three kinds of
/// wrong: it would silently skip files excluded by the variant's build
/// constraints (`foo_windows.go` never formatted unless a windows variant is
/// declared), run N near-identical jobs for N variants, and have several variants
/// claim the same `codegen = in_place` source paths. So they source their file
/// list straight off disk (every `*.go` in the package dir) instead of from the
/// variant-scoped `_golist`.
///
/// The lint targets are *not* in here: they carry no variant either, but they
/// reach the per-variant analysis units underneath, so they need their own
/// handling (see [`VARIANT_AGGREGATE_TARGET_NAMES`]).
const VARIANT_FREE_TARGET_NAMES: &[&str] = &["format", "format-check"];

/// Target names that carry no `@v`/`@vp` themselves but **fan out over every
/// declared variant** underneath — one bare addr per package aggregating N
/// per-variant analysis units.
///
/// Lint *rules* are variant-independent, but the object they analyze is not:
/// `_lint-analyze` type-checks the package, and a typed package only exists per
/// `(GOOS, GOARCH, tags)` — `foo_linux.go` and `foo_windows.go` redeclare each
/// other's symbols, and their imports differ. Facts are variant-scoped for the
/// same reason. So the analysis unit stays per-variant while the user-facing
/// gate (`lint-check`) and fixer (`lint`) aggregate the module-bounded ancestry
/// variants.
///
/// That fixes two things a variant-selected `lint` got wrong: a file excluded by
/// the selected variant's build constraints was silently never linted
/// (`foo_windows.go` on a host-variant run), and N per-variant fixers each
/// declared the same `codegen = in_place` source paths — N targets racing to
/// rewrite one file. One aggregating fixer claims each path once.
const VARIANT_AGGREGATE_TARGET_NAMES: &[&str] = &["lint-check", "lint"];

/// Workspace-relative package of heph's own go/analysis unitchecker binary
/// (`heph-govet`), built as an ordinary `build` (package main) target like any
/// other first-party binary. It exists only in heph's repo — which is why the
/// `govet` option points at it from *there* (`govet = "//tools/heph-govet:build"`)
/// while every other workspace uses the default download target (see [`govet`]).
/// Referenced by the tests that exercise the from-source flavor.
#[cfg(test)]
const GOVET_TOOL_PKG: &str = "tools/heph-govet";
/// The `govet` option heph's own repo (and this crate's tests) uses: build the
/// tool from source rather than download a release that a dev build has none of.
#[cfg(test)]
const GOVET_SOURCE_ADDR: &str = "//tools/heph-govet:build";

/// First-party iff the dep package is neither stdlib (`@heph/go/std/…`) nor
/// thirdparty (`…@heph/go/thirdparty/…`). `go_lint` generates facts deps only
/// for first-party packages: std/thirdparty contribute export-data (type info)
/// via their `build_lib` archives but are not linted, so interprocedural
/// analyzers reason fully across first-party boundaries and fall back to
/// intra-package across std/thirdparty edges.
fn is_firstparty_pkg(pkg: &str) -> bool {
    !pkg.starts_with("@heph/go/std/") && !pkg.contains("@heph/go/thirdparty/")
}

/// Whether this provider owns `name` — the complete set of go targets it can
/// generate: the pre-golist special targets, the `_golist`-resolved non-test
/// targets, and every test variant.
fn is_known_go_target_name(name: &str) -> bool {
    SPECIAL_TARGET_NAMES.contains(&name)
        || GOLIST_TARGET_NAMES.contains(&name)
        || VARIANT_FREE_TARGET_NAMES.contains(&name)
        || VARIANT_AGGREGATE_TARGET_NAMES.contains(&name)
        || TEST_TARGET_NAMES.contains(&name)
}

/// Choose which lib variant of P (the current package) to expose in the
/// xtest_lib compile and the build_xtest link, so both agree.
///
/// xtest bin's importcfg must reference the SAME .a for P that xtest_lib's
/// compile embedded — otherwise the linker rejects the fingerprint mismatch.
///
/// - GoFiles non-empty → `build_lib` (normal). Required by the cycle-safety
///   rule documented on `build_xtest`: all consumers of P agree on normal.
/// - GoFiles empty, TestGoFiles non-empty → `build_test_lib`. test-only and
///   test+xtest-only packages have no normal lib, so the test-augmented lib
///   is the only available flavor; no cycle is possible because nothing else
///   in the build can import P (it has no buildable non-test sources).
/// - both empty (xtest-only) → `None`. P has no lib at all; xtest source
///   can't legitimately reference symbols from a package with no declarations.
fn pick_xtest_p_lib_name(pkg: &GoPackage) -> Option<&'static str> {
    if !pkg.go_files.is_empty() {
        Some("build_lib")
    } else if !pkg.test_go_files.is_empty() {
        Some("build_test_lib")
    } else {
        None
    }
}

/// Return true if the closest `test` state applying to `addr_pkg` disables tests
/// via `test = False`. Like the other per-package knobs, a `test` state applies
/// to its exact package by default and reaches descendants only when it carries
/// `recursive = True`. Deeper states fully override shallower ones — a
/// `test = True` (or the struct form, which implies tests run) closer to the
/// target re-enables tests even if a recursive ancestor disabled them.
fn pick_test_skip(states: &[State], addr_pkg: &str) -> bool {
    let Some(state) = applicable_states(states, addr_pkg, "test")
        .into_iter()
        .last()
    else {
        return false;
    };
    // Skip only when the closest `test` state is the bool `False`. `True` and the
    // struct form (env config) both leave tests enabled.
    matches!(state.state.get("test"), Some(Value::Bool(false)))
}

/// Parse a `map[string]string` Starlark value into a sorted map. Rejects
/// non-string values so a typo (e.g. an int) surfaces as a clear error rather
/// than being silently dropped.
fn parse_str_map(v: &Value) -> anyhow::Result<BTreeMap<String, String>> {
    let Value::Map(m) = v else {
        anyhow::bail!("expected map[string], got: {v:?}");
    };
    m.iter()
        .map(|(k, val)| match val {
            Value::String(s) => Ok((k.clone(), s.clone())),
            other => Err(anyhow::anyhow!(
                "expected string value for key `{k}`, got: {other:?}"
            )),
        })
        .collect()
}

/// Collect the test env knobs (`env`, `runtime_env`, `pass_env`,
/// `runtime_pass_env`) from a `test = {...}` provider_state declared in the
/// *exact* package of the target.
///
/// Unlike `skip` (which inherits down the package tree via [`pick_test_skip`]),
/// these apply only to the package that declares them — a state at `//foo` does
/// not leak into `//foo/bar`'s test targets. The engine pre-filters states by
/// provider name, so callers only see Go states.
/// Keys accepted inside a `test = {...}` go provider_state map. Used to reject
/// typos / unsupported knobs (the `test` map only configures env, never
/// enable/disable — that's the bool `test = False`).
const TEST_STATE_KEYS: &[&str] = &[
    "env",
    "runtime_env",
    "pass_env",
    "runtime_pass_env",
    "pre_run",
];

/// Whether a state opts its per-package config into descendant packages via
/// `recursive = True`. Engine pre-filters states to ancestors-of-or-equal-to the
/// target package, so a `recursive` state is always a valid ancestor (or self).
fn state_is_recursive(state: &State) -> bool {
    matches!(state.state.get("recursive"), Some(Value::Bool(true)))
}

/// Whether a state's per-package config (the `test = {...}` struct and
/// `link = {...}`) applies to `addr_pkg`. By default config applies only to the
/// exact declaring package; `recursive = True` extends it to all descendants.
fn state_applies_to(state: &State, addr_pkg: &str) -> bool {
    state.package.as_str() == addr_pkg || state_is_recursive(state)
}

/// Return the `states` that apply to `addr_pkg` (exact package, or `recursive`
/// ancestors) and carry `key`, sorted shallow->deep so the closest declaration
/// is applied last and wins on conflicting map keys.
fn applicable_states<'a>(states: &'a [State], addr_pkg: &str, key: &str) -> Vec<&'a State> {
    let mut out: Vec<&State> = states
        .iter()
        .filter(|s| state_applies_to(s, addr_pkg) && s.state.contains_key(key))
        .collect();
    out.sort_by_key(|s| s.package.as_str().len());
    out
}

fn pick_test_env(states: &[State], addr_pkg: &str) -> anyhow::Result<target_test::TestEnv> {
    let mut out = target_test::TestEnv::default();
    for state in applicable_states(states, addr_pkg, "test") {
        let Some(Value::Map(test_map)) = state.state.get("test") else {
            continue;
        };
        // Reject typos / unsupported knobs instead of silently dropping them —
        // e.g. `test = {"skip": True}` (tests are disabled with `test = False`,
        // not a `skip` key). Fail closed so the BUILD author sees the mistake.
        for key in test_map.keys() {
            if !TEST_STATE_KEYS.contains(&key.as_str()) {
                anyhow::bail!(
                    "unknown key `{key}` in go provider_state `test` map (allowed: {}); \
                     to disable tests use `test = False`",
                    TEST_STATE_KEYS.join(", "),
                );
            }
        }
        if let Some(v) = test_map.get("env") {
            out.env
                .extend(parse_str_map(v).context("parsing test env from go provider_state")?);
        }
        if let Some(v) = test_map.get("runtime_env") {
            out.runtime_env.extend(
                parse_str_map(v).context("parsing test runtime_env from go provider_state")?,
            );
        }
        if let Some(v) = test_map.get("pass_env") {
            out.pass_env
                .extend(parse_strings(v).context("parsing test pass_env from go provider_state")?);
        }
        if let Some(v) = test_map.get("runtime_pass_env") {
            out.runtime_pass_env.extend(
                parse_strings(v).context("parsing test runtime_pass_env from go provider_state")?,
            );
        }
        if let Some(v) = test_map.get("pre_run") {
            out.pre_run
                .extend(parse_strings(v).context("parsing test pre_run from go provider_state")?);
        }
    }
    Ok(out)
}

/// Keys accepted inside a `link = {...}` go provider_state map. Rejects typos /
/// unsupported knobs instead of silently dropping them.
const LINK_STATE_KEYS: &[&str] = &["flags", "deps", "runtime_deps"];

/// Schema for `link` `deps`/`runtime_deps`: a list of addresses, or a
/// `{group: addr | [addr, …]}` map naming dep groups. Mirrors the exec plugin's
/// `deps` union so BUILD authors get the same shape everywhere.
fn link_deps_param_type() -> ParamType {
    let str_or_list = ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
    ParamType::union(vec![
        ParamType::String,
        ParamType::list(ParamType::String),
        ParamType::map(str_or_list),
    ])
}

/// Merge a parsed `{group: [addr, …]}` map into a link dep accumulator so that
/// recursive ancestor states and the package's own state combine per group.
fn extend_link_deps(out: &mut BTreeMap<String, Vec<String>>, v: &Value) -> anyhow::Result<()> {
    for (group, addrs) in parse_map_string_strings(v)? {
        out.entry(group).or_default().extend(addrs);
    }
    Ok(())
}

/// Collect the link knobs (`flags`, `deps`, `runtime_deps`) from `link = {...}`
/// provider_states applying to the binary's package — its own package plus any
/// `recursive` ancestor. Applicable states accumulate (shallow->deep), so a
/// recursive ancestor's flags/deps combine with the package's own.
fn pick_link(states: &[State], addr_pkg: &str) -> anyhow::Result<target_bin::LinkConfig> {
    let mut out = target_bin::LinkConfig::default();
    for state in applicable_states(states, addr_pkg, "link") {
        let Some(Value::Map(link_map)) = state.state.get("link") else {
            anyhow::bail!(
                "go provider_state `link` must be a struct, got: {:?}",
                state.state.get("link")
            );
        };
        for key in link_map.keys() {
            if !LINK_STATE_KEYS.contains(&key.as_str()) {
                anyhow::bail!(
                    "unknown key `{key}` in go provider_state `link` map (allowed: {})",
                    LINK_STATE_KEYS.join(", "),
                );
            }
        }
        if let Some(v) = link_map.get("flags") {
            out.flags
                .extend(parse_strings(v).context("parsing link flags from go provider_state")?);
        }
        if let Some(v) = link_map.get("deps") {
            extend_link_deps(&mut out.deps, v)
                .context("parsing link deps from go provider_state")?;
        }
        if let Some(v) = link_map.get("runtime_deps") {
            extend_link_deps(&mut out.runtime_deps, v)
                .context("parsing link runtime_deps from go provider_state")?;
        }
    }
    Ok(out)
}

/// Pick the closest (deepest) ancestor state carrying `go_codegen_deps`.
/// Independent of `go_codegen_root` — a BUILD file declaring only
/// `go_codegen_deps` must still inject those deps into descendant `_golist`
/// targets so generated `.go` files reach the sandbox. Mirrors `getCodegenDeps`
/// in the Go reference impl (`heph/plugin/plugingo/plugin.go:184-200`), which
/// scans for deps independently of the root marker.
fn pick_codegen_deps(states: &[State]) -> Option<&State> {
    states
        .iter()
        .filter(|s| s.state.contains_key("go_codegen_deps"))
        .max_by_key(|s| s.package.as_str().len())
}

/// Source addrs the package sandbox needs beyond the canonical `*.go` filesystem
/// glob. Includes:
/// 1. `**/*` filesystem glob (excluding `.go` files) — picks up checked-in
///    non-Go sources (e.g. embed targets).
/// 2. `query("… && label(go_src) && tree_output(pkg)")` — unpacks the full output tree
///    of any codegen target labelled `go_src` into the pkg dir, so both
///    generated `.go` files and any sibling non-go outputs (e.g. `.wasm.br`)
///    land in the sandbox.
/// 3. `go_codegen_deps` from the closest ancestor BUILD state — explicit
///    codegen targets that don't carry the `go_src` label.
///
/// Shared between `_golist` (so `go list` can resolve `//go:embed` patterns
/// into `EmbedFiles`) and `embed` (so the driver's runtime re-glob of
/// `embed_patterns` against `sandbox_pkg_dir` matches Go's resolution).
/// Query-language pattern selecting exactly the package `pkg` (`//` for root).
fn pkg_pattern(pkg: &str) -> String {
    format!("//{pkg}")
}

/// Default query scope for a package's source/embed lanes, when no codegen root is
/// declared: the package's **subtree**, so a generator sitting in a sub-package of
/// the consuming Go package is in scope (`app/openapi` bundling a spec that `app`
/// embeds). `tree_output(pkg)` still gates membership, so the wider scope cannot
/// admit a target whose output lands elsewhere.
///
/// The **root** package is the exception: its subtree is the entire workspace,
/// which includes the synthetic provider namespaces (`//@heph/…`) that are not
/// source packages at all — resolving every target in the repo to a def just to
/// answer this query is both wasteful and, with foreign names in play, wrong. A
/// root-level Go package that generates into a sub-package must declare a codegen
/// root (which is exactly what that option is for).
fn default_scope(pkg: &str) -> String {
    if pkg.is_empty() {
        pkg_pattern(pkg)
    } else {
        pkg_prefix_pattern(pkg)
    }
}

/// Query-language pattern selecting every package under `pkg` (`//...` for root).
fn pkg_prefix_pattern(pkg: &str) -> String {
    if pkg.is_empty() {
        "//...".to_string()
    } else {
        format!("//{pkg}/...")
    }
}

/// The `@heph/query` addr selecting `go_test_data`-labelled targets in `pkg`.
///
/// Excludes the `go` provider: these labels are carried by buildfile-emitted
/// codegen targets, never by go targets. Skipping the go provider avoids
/// resolving (and cascade-building) the go provider's own variant-parameterized
/// targets — which are multiplied per variant and whose spec resolution pulls in
/// the whole per-variant golist/std graph.
fn go_test_data_query_addr(pkg: &str) -> Addr {
    let expr = format!("{} && label(go_test_data)", pkg_pattern(pkg));
    hplugin_query::pluginquery::query_addr(&expr, "", &["go"])
}

fn compute_pkg_src_addrs(pkg_str: &str, states: &[State]) -> anyhow::Result<Vec<String>> {
    let non_go_glob = if pkg_str.is_empty() {
        "**/*".to_string()
    } else {
        format!("{}/**/*", pkg_str)
    };
    // Exclude `.go` (their own lane) and the module files `go.mod`/`go.sum`:
    // the latter are delivered into `_golist` by the modfiles (`_go_mod`) lane
    // as `fs:file` deps. Without excluding them here the non-go glob and the
    // modfiles dep both produce `go.mod`/`go.sum` in the sandbox — two
    // different targets writing the same file, which the sandbox runner now
    // rejects as an output collision.
    let non_go_glob_addr =
        pluginfs::glob_addr(&non_go_glob, &["**/*.go", "**/go.mod", "**/go.sum"]);
    let mut addrs = vec![non_go_glob_addr.format()];

    let codegen_root = pick_codegen_root(states);
    // Scope: every package under the codegen root if one is declared, else the
    // target's own package *and everything under it*.
    //
    // The subtree — not the bare package — is the floor: a generator commonly sits
    // in a sub-package of the Go package that consumes its output (`app/openapi`
    // bundling a spec into `app/openapi/openapi_gen.yaml`, embedded by `app`).
    // Its output lands inside the consumer's tree, so `tree_output` matches it, but
    // a `//pkg`-only scope rejects it before that term is ever reached — and the fs
    // glob can't cover for it either, since codegen-stamped files are skipped
    // there. The file then reached `go list` only when its on-disk copy happened to
    // be unstamped, which is how this surfaced: an embed that resolved on some runs
    // and failed with "//go:embed pattern(s) matched no files" on others.
    //
    // Widening the floor cannot pull in a foreign target: `tree_output(pkg)` still
    // requires the output to land in *this* package's tree.
    let scope = match codegen_root {
        Some(root) => pkg_prefix_pattern(root.package.as_str()),
        None => default_scope(pkg_str),
    };
    // Cheapest-first by resolution tier: `scope` resolves at the no-IO addr
    // tier, `label` at the spec tier (`get_spec`), and `tree_output` only at the
    // def tier (`get_def`, the most expensive). Order terms by that cost so the
    // engine's left-to-right `&&` bails at the cheapest possible tier.
    // Exclude the `go` provider: `go_src` labels only its buildfile codegen
    // targets, never go targets — skipping go avoids resolving (and cascade-
    // building) the go provider's per-variant targets.
    let go_src_expr = format!("{scope} && label(go_src) && tree_output({pkg_str})");
    let go_src_query_addr = hplugin_query::pluginquery::query_addr(&go_src_expr, "", &["go"]);
    addrs.push(go_src_query_addr.format());

    if let Some(deps_state) = pick_codegen_deps(states)
        && let Some(deps_val) = deps_state.state.get("go_codegen_deps")
    {
        let deps =
            parse_strings(deps_val).context("parsing go_codegen_deps from go provider_state")?;
        addrs.extend(deps);
    }
    Ok(addrs)
}

/// Pick the closest (deepest) ancestor state carrying `go_embed_deps`. Mirrors
/// [`pick_codegen_deps`] for the embed-only lane.
fn pick_embed_deps(states: &[State]) -> Option<&State> {
    states
        .iter()
        .filter(|s| s.state.contains_key("go_embed_deps"))
        .max_by_key(|s| s.package.as_str().len())
}

/// Address set for the `go_embed_src` lane: targets whose outputs are consumed
/// *only* via `//go:embed`, never parsed by `go list`.
///
/// Deliberately **excluded from `_golist`** (unlike [`compute_pkg_src_addrs`]),
/// so an expensive asset build — e.g. a frontend bundle — never blocks
/// list/query/metadata or `go list` itself. The patterns are still reported by
/// `go list` (parsed from the `.go` source); the `go_embed` driver resolves them
/// against these staged files downstream, and `build_lib` stages them for the
/// compile. Sources:
/// 1. `query("… && label(go_embed_src) && tree_output(pkg)")` — codegen targets
///    labelled `go_embed_src`.
/// 2. `go_embed_deps` from the closest ancestor BUILD state — explicit embed
///    targets that don't carry the label.
fn compute_embed_src_addrs(pkg_str: &str, states: &[State]) -> anyhow::Result<Vec<String>> {
    let codegen_root = pick_codegen_root(states);
    // Same floor as the go_src lane (see `compute_pkg_src_addrs`): the subtree, so a
    // generator in a sub-package of the embedding package is in scope.
    let scope = match codegen_root {
        Some(root) => pkg_prefix_pattern(root.package.as_str()),
        None => default_scope(pkg_str),
    };
    // Cheapest-first by resolution tier (see `compute_pkg_src_addrs`): `scope`
    // (addr) < `label` (spec/`get_spec`) < `tree_output` (def/`get_def`).
    // Exclude the `go` provider (see `compute_pkg_src_addrs`): `go_embed_src`
    // only labels buildfile codegen targets.
    let expr = format!("{scope} && label(go_embed_src) && tree_output({pkg_str})");
    let query_addr = hplugin_query::pluginquery::query_addr(&expr, "", &["go"]);
    let mut addrs = vec![query_addr.format()];

    if let Some(deps_state) = pick_embed_deps(states)
        && let Some(deps_val) = deps_state.state.get("go_embed_deps")
    {
        let deps =
            parse_strings(deps_val).context("parsing go_embed_deps from go provider_state")?;
        addrs.extend(deps);
    }
    Ok(addrs)
}

/// The package's static (checked-in) non-Go source tree as a single fs-glob addr,
/// excluding `.go`. Staged into the compile's `embed_src` group when `go list`'s
/// `EmbedFiles` came back empty — which happens when an unresolved `go_embed_src`
/// pattern (decoupled out of `_golist`) poisons go list's atomic per-package embed
/// resolution, zeroing `EmbedFiles` for the co-located plain `//go:embed` statics
/// too. Staging the static tree lets the in-driver Go-faithful selector resolve
/// those statics from the bytes. Generated `go_embed_src` outputs carry the
/// codegen xattr and are skipped by the glob, so the decoupling (and any
/// `go_embed_src` lane staging) is preserved with no double-staging.
fn pkg_static_embed_glob_addr(pkg_str: &str) -> String {
    let glob = if pkg_str.is_empty() {
        "**/*".to_string()
    } else {
        format!("{pkg_str}/**/*")
    };
    pluginfs::glob_addr(&glob, &["**/*.go"]).format()
}

impl ProviderInner {
    async fn handle_get(self: Arc<Self>, req: GetRequest) -> Result<GetResponse, GetError> {
        let addr = &req.addr;

        // Hermetic Go toolchain: `//@heph/go/toolchain/<version>:go` downloads
        // the pinned SDK for the host platform. This replaces the former
        // host-PATH go.
        if addr.name == toolchain::TOOLCHAIN_NAME
            && let Some(version) = toolchain::version_from_pkg(addr.package.as_str())
        {
            let (goos, goarch) = (current_goos(), current_goarch());
            let key = toolchain::checksum_key(version, &goos, &goarch);
            // Checksum is optional: absent → unverified download (the driver
            // warns). An empty sha256 threads through to the toolchain target.
            let sha256 = self
                .sdk_checksums
                .get(&key)
                .map(String::as_str)
                .unwrap_or("");
            let spec = toolchain::build_spec(addr.clone(), version, &goos, &goarch, sha256);
            return Ok(GetResponse { target_spec: spec });
        }

        // `heph-govet`, the analysis/format binary the lint and format targets
        // exec: `//@heph/go/govet/<tag>:heph-govet` is an `http_fetch` over the
        // asset published in heph release `<tag>` (the URL templates over this
        // addr's `goos`/`goarch` args). This is what the `govet` option defaults
        // to; pointing it at a build target instead never reaches here.
        if addr.name == govet::GOVET_NAME
            && let Some(tag) = govet::tag_from_pkg(addr.package.as_str())
        {
            // A dev build's default tag names a release that was never published.
            // Diagnosed here — where something actually asks for the tool — so that
            // merely listing a lint target's spec still works on a dev build: a
            // `query` / `//...` walk asks every target for its spec, and that must
            // not require owning a heph-govet.
            if govet::is_dev_tag(tag) {
                return Err(GetError::Other(anyhow::anyhow!(
                    "go provider: this is a dev build of heph ({tag}), and no release publishes \
                     a heph-govet binary for it — set the `govet` option to a source build \
                     (\"//tools/heph-govet:build\") or to a released tag's download target \
                     (\"//@heph/go/govet/<tag>:heph-govet\")"
                )));
            }
            // The govet tool is host-native (never cross-compiled), so its asset
            // is keyed by the host platform — read the host factors directly, not a
            // build variant.
            let (goos, goarch) = (current_goos(), current_goarch());
            let sha256 = govet::expected_sha256(&self.sdk_checksums, tag, &goos, &goarch);
            let spec = govet::build_spec(addr.clone(), tag, &sha256);
            return Ok(GetResponse { target_spec: spec });
        }

        // Standard library install: `@heph/go/std:install` builds all of std from
        // source once (per variant); per-package `build_lib` extracts archives from
        // its output. Lives at the bare `@heph/go/std` package, which does not
        // decode as a Stdlib import path, so handle it before `decode_package`. It
        // is variant-parameterized (std is compiled with the variant's factors), so
        // resolve the variant here.
        if addr.package.as_str() == target_std::STD_PKG && addr.name == "install" {
            // std:install is always internal (carries `vp`), so it takes the
            // library/universe branch — `module_root` is unused. std belongs to
            // no user module; its `vp` (the consumer's declaring package) is
            // always honored so std builds with the consumer's variant factors.
            let (factors, _vref) =
                variant::resolve(addr, &req.states, "", req.executor.as_ref(), true)
                    .await
                    .map_err(GetError::Other)?;
            let spec = target_std::install_spec(addr.clone(), &factors, &self.go_version);
            return Ok(GetResponse { target_spec: spec });
        }

        let kind = match decode_package(&addr.package, &self.workspace_root) {
            Some(k) => k,
            None => return Err(GetError::NotFound),
        };

        // A first-party package inside a skipped subtree does not resolve —
        // mirroring what the package walk (`collect_go_packages`) prunes.
        if matches!(&*kind, GoPackageKind::FirstParty { .. })
            && self
                .skip
                .prunes_package(&self.workspace_root, Path::new(addr.package.as_str()))
        {
            return Err(GetError::NotFound);
        }

        // Reject names this provider doesn't own as early as possible — before any
        // special-case handler or `go list`. A foreign name (e.g. a buildfile
        // codegen target sharing a Go package dir) would otherwise drag `go list`
        // and its `label(go_src)` query into resolution and trip a cycle. On by
        // default (perf/clarity); the engine contains the cycle regardless (cyclic
        // provider attempts fall through to the next provider), so tests exercising
        // that path disable it via `Config::foreign_name_guard`. Owned names —
        // including the specials handled just below (`_golist`/`_go_mod`/`download`)
        // — are in `is_known_go_target_name`, so this never rejects a real target.
        if self.foreign_name_guard && !is_known_go_target_name(&addr.name) {
            return Err(GetError::NotFound);
        }

        // _go_mod — copy go.mod/go.sum; variant-independent, so handle it before
        // variant resolution (its addr carries no `v`).
        if addr.name == "_go_mod" {
            let module_root = self.workspace_root.join(addr.package.as_str());
            let mod_files: Vec<String> = ["go.mod", "go.sum"]
                .iter()
                .filter(|f| module_root.join(f).exists())
                .map(|f| f.to_string())
                .collect();
            if mod_files.is_empty() {
                return Err(GetError::NotFound);
            }
            let spec = target_modfiles::build_spec(addr.clone(), &mod_files);
            return Ok(GetResponse { target_spec: spec });
        }

        // download — module-root only, variant-independent (module bytes don't
        // depend on the build variant). Runs `go mod download` and exposes the
        // module source tree as artifacts so downstream build_lib / embed targets
        // get fully sandboxed sources instead of host GOMODCACHE. Handle before
        // variant resolution (its addr carries no `v`).
        if addr.name == "download" {
            if let GoPackageKind::ThirdParty {
                module,
                version,
                subpath,
                ..
            } = &*kind
            {
                if !subpath.is_empty() {
                    return Err(GetError::NotFound);
                }
                let spec = thirdparty::build_download_spec(
                    addr.clone(),
                    module,
                    version,
                    &self.go_version,
                );
                return Ok(GetResponse { target_spec: spec });
            }
            return Err(GetError::NotFound);
        }

        // A name this provider doesn't own can't be a variant target — decline so
        // the engine falls through to the owning provider. `foreign_name_guard`
        // above is the on-by-default fast path (before `decode_package`); this
        // unconditional check covers the guard-off case, where a foreign name (e.g.
        // a buildfile codegen target sharing a Go package dir, spec-resolved via a
        // go-first registration) must decline rather than error out of variant
        // resolution below.
        if !is_known_go_target_name(&addr.name) {
            return Err(GetError::NotFound);
        }

        // Everything below is variant-parameterized. Resolve the addr's variant —
        // a bare `@v` (binary/entry target) resolves by module-bounded ancestry;
        // `@v,vp` (library / dependency target) resolves `(name, vp)` against the
        // module universe. Returns the concrete `factors` this target builds with,
        // plus the `vref` to thread onto every sub-target and dependency address.
        //
        // `module_root` bounds ancestry resolution at the go.mod dir (not repo
        // root); it is unused for the `vp` (library) branch.
        let module_root = module_root_rel(&kind, &self.workspace_root);

        // format / format-check — variant-free (see `VARIANT_FREE_TARGET_NAMES`),
        // so handle them before variant resolution and source the file list from
        // disk rather than the variant-scoped `_golist`.
        if VARIANT_FREE_TARGET_NAMES.contains(&addr.name.as_str()) {
            return match self.get_format(addr, &kind).map_err(GetError::Other)? {
                Some(resp) => Ok(resp),
                None => Err(GetError::NotFound),
            };
        }

        // lint-check / lint — one bare addr per package aggregating the
        // per-variant `_lint-analyze` units (see `VARIANT_AGGREGATE_TARGET_NAMES`).
        // They carry no variant of their own, so they also resolve before variant
        // resolution; they enumerate the ancestry variants themselves.
        if VARIANT_AGGREGATE_TARGET_NAMES.contains(&addr.name.as_str()) {
            return Arc::clone(&self)
                .get_lint_aggregate(&req, &kind, &module_root)
                .await;
        }

        // Magic host-default `build`: a *truly bare* `//pkg:build` (no addr args at
        // all) is served as a `group` target forwarding to `build@v=<variant>` for
        // the first ancestry variant matching this machine's goos/goarch. It gives
        // `//pkg:build` an ergonomic host default without an implicit variant on the
        // real per-variant build. First-party only — `build` (and thus its
        // host-default) exists solely for first-party packages, matching what `list`
        // emits.
        //
        // Gate on `args.is_empty()`, NOT merely "no `v`": a `build` carrying stray
        // args — e.g. a legacy `build@goos=linux,goarch=amd64` from a pre-variant
        // BUILD file — must fall through to `variant::resolve`, which reports a clear
        // "requires `@v=NAME`" migration error, rather than being silently treated as
        // the host-default (which would forward to the host variant and confusingly
        // NotFound for a target-platform-only package).
        if addr.name == "build"
            && addr.args.is_empty()
            && matches!(&*kind, GoPackageKind::FirstParty { .. })
        {
            let (host_goos, host_goarch) = (current_goos(), current_goarch());
            let chosen = variant::ancestry_variants_with_factors(&req.states, &module_root)
                .into_iter()
                .find(|(_, f)| f.goos == host_goos && f.goarch == host_goarch);
            let Some((vref, _)) = chosen else {
                return Err(GetError::NotFound);
            };
            // The magic target must resolve exactly where the real `build` does —
            // a `main` package. A non-main / library / directory-only package (no
            // buildable Go files) has no `build`, so decline here on the *magic*
            // addr rather than emit a `group` whose `build@v=…` dep can't resolve.
            // Declining on the self addr lets the codegen/query/`validate` walks
            // skip it (they match a self-addr `TargetNotFound`); a cross-addr dep
            // NotFound would surface instead. Checked via the chosen variant's
            // `_golist` (engine-cached; the real `build@v=…` reads the same one).
            let golist_addr = self.make_addr_with_name(&addr.package, "_golist", &vref);
            match self
                .read_golist_package(Arc::clone(&req.executor), &golist_addr)
                .await
            {
                Ok(pkg) if pkg.name.as_deref() == Some("main") => {}
                Ok(_) => return Err(GetError::NotFound),
                Err(e) if downcast_chain_ref::<NoGoFilesError>(&e).is_some() => {
                    return Err(GetError::NotFound);
                }
                Err(e) => return Err(GetError::Other(e)),
            }
            let target = Addr::new(
                addr.package.clone(),
                "build".to_string(),
                BTreeMap::from([("v".to_string(), vref.name.clone())]),
            );
            return Ok(GetResponse {
                target_spec: magic_build_group_spec(addr.clone(), &target),
            });
        }

        // Strict addr args: every variant-parameterized go target accepts only `v`
        // (entry) and `vp` (library/dep pin). Reject anything else rather than
        // silently ignoring it — notably a legacy `goos`/`goarch` from a pre-variant
        // BUILD file, which must surface as an actionable error, not resolve to the
        // wrong thing. (The host-keyed toolchain/govet targets, which do carry
        // `goos`/`goarch`, are handled earlier and never reach here.)
        if let Some(bad) = addr.args.keys().find(|k| !matches!(k.as_str(), "v" | "vp")) {
            return Err(GetError::Other(anyhow::anyhow!(
                "unknown addr arg `{bad}` on go target `:{}` (allowed: v, vp); \
                 select a build variant with `@v=NAME` — `goos`/`goarch` are no \
                 longer addr args, declare a variant instead",
                addr.name,
            )));
        }

        // Decide whether to honor the addr's `vp`:
        //   - std / thirdparty belong to *no* user module — they carry no
        //     variant declarations of their own and are only ever reached as a
        //     dependency. Their factors ARE the consumer's, threaded via `vp`, so
        //     always honor it (module-bounding them would strand them with an
        //     empty ancestry and fail).
        //   - first-party: honor `vp` only when it names a package in *this*
        //     target's own go module (a real nearest-`go.mod` check, not a path
        //     prefix). A `vp` threaded from a consumer in a different module is a
        //     cross-module dep pin; ignoring it keeps the foreign module's
        //     variant declaration from leaking across the boundary.
        let vp_same_module = match &*kind {
            GoPackageKind::Stdlib { .. } | GoPackageKind::ThirdParty { .. } => true,
            GoPackageKind::FirstParty { .. } => addr
                .args
                .get("vp")
                .is_some_and(|vp| pkg_in_module(vp, &module_root, &self.workspace_root)),
        };
        let (factors, vref) = variant::resolve(
            addr,
            &req.states,
            &module_root,
            req.executor.as_ref(),
            vp_same_module,
        )
        .await
        .map_err(GetError::Other)?;

        // _golist — generate spec without executing go list (before stdlib check so
        // stdlib packages can also expose a _golist target for cached dep resolution)
        if addr.name == "_golist" {
            return self
                .get_golist_spec(addr.clone(), &kind, &factors, &req.states)
                .map_err(GetError::Other);
        }

        // Stdlib — no go list needed for other targets
        if let GoPackageKind::Stdlib { import_path } = &*kind {
            return self.get_stdlib(addr.clone(), import_path, &factors, &vref);
        }

        // provider_state(provider="go", test=False) opts the package (and all
        // descendants) out of test-target generation. Gate every test variant
        // before the `_golist` resolve below so a skipped pkg never forces a
        // `go list` round-trip purely to learn there are no tests.
        if is_test_target_name(&addr.name) && pick_test_skip(&req.states, addr.package.as_str()) {
            return Err(GetError::NotFound);
        }

        let import_path = match &*kind {
            GoPackageKind::FirstParty { import_path, .. } => import_path.clone(),
            GoPackageKind::ThirdParty {
                module, subpath, ..
            } => {
                if subpath.is_empty() {
                    module.clone()
                } else {
                    format!("{}/{}", module, subpath)
                }
            }
            GoPackageKind::Stdlib { .. } => return Err(GetError::NotFound),
        };

        // Resolve package info via _golist target (cached by the engine).
        // `NoGoFilesError` (raised by `read_golist_package` when `go list -e`
        // reports the package has no buildable Go files) maps uniformly to
        // `NotFound` for every variant — no per-arm duck-typed check needed.
        let golist_addr = self.make_addr_with_name(&addr.package, "_golist", &vref);
        let pkg = match self
            .read_golist_package(Arc::clone(&req.executor), &golist_addr)
            .await
        {
            Ok(p) => p,
            Err(e) if downcast_chain_ref::<NoGoFilesError>(&e).is_some() => {
                return Err(GetError::NotFound);
            }
            Err(e) => return Err(GetError::Other(e)),
        };

        if pkg.dir.is_none() {
            return Err(GetError::Other(anyhow::anyhow!(
                "package '{}' has no Dir",
                import_path
            )));
        }

        // The module root drives which directory `go list` runs from for transitive deps.
        let module_root = match &*kind {
            GoPackageKind::FirstParty { module_root, .. } => module_root.clone(),
            GoPackageKind::ThirdParty { module_root, .. } => module_root.clone(),
            GoPackageKind::Stdlib { .. } => return Err(GetError::NotFound),
        };

        match addr.name.as_str() {
            "build_lib" => {
                // A library with no Go source files isn't buildable. Previously
                // caught by a shared `go_files.is_empty() && error.is_some()`
                // guard above; now the sentinel only fires on the NOGO case
                // (`error.is_some()`), so the no-source-but-test-files case
                // (e.g. test-only packages) needs its own guard here.
                if pkg.go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(Arc::clone(&req.executor), &pkg, &[], &vref, &module_root)
                    .await
                    .map_err(GetError::Other)?;

                // The package embeds iff `go list` reported any embed pattern/file.
                // `go_compile` resolves the embedcfg in-process from `_golist`'s
                // package.bin — no separate `go_embed` target. Pass the golist
                // addr only when embedding.
                let embedding = !pkg.embed_patterns.is_empty() || !pkg.embed_files.is_empty();
                let embed_golist = if embedding { Some(&golist_addr) } else { None };

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                let spec = match &*kind {
                    GoPackageKind::ThirdParty { .. } => thirdparty::build_lib_spec(
                        addr.clone(),
                        &pkg,
                        &factors,
                        &transitive.libs,
                        &pkg_addrs.go_files,
                        &pkg_addrs.s_files,
                        &pkg_addrs.h_files,
                        &pkg_addrs.extra_h_files,
                        embed_golist,
                        &pkg_addrs.embed_files,
                        &self.go_version,
                    ),
                    _ => {
                        // `go_embed_src` assets (kept out of `_golist`) are staged
                        // for the compile so the in-driver embedcfg finds the bytes.
                        let mut embed_src_addrs = if embedding {
                            compute_embed_src_addrs(addr.package.as_str(), &req.states)
                                .map_err(GetError::Other)?
                        } else {
                            Vec::new()
                        };
                        // When `go list` resolved zero EmbedFiles but the package
                        // embeds, a go_embed_src pattern poisoned go list's atomic
                        // resolution and zeroed the co-located static embeds too.
                        // Stage the static non-go tree so the selector resolves them.
                        if embedding && pkg_addrs.embed_files.is_empty() {
                            embed_src_addrs.push(pkg_static_embed_glob_addr(addr.package.as_str()));
                        }
                        target_lib::build_spec(
                            addr.clone(),
                            &import_path,
                            pkg.name.as_deref().unwrap_or(""),
                            &factors,
                            &transitive.libs,
                            &pkg_addrs.go_files,
                            &self.go_version,
                            embed_golist,
                            &pkg_addrs.embed_files,
                            &embed_src_addrs,
                        )
                    }
                };
                Ok(GetResponse { target_spec: spec })
            }
            // Analyze unit: runs heph-govet, produces `lint.facts` (consumed by
            // dependents) + `lint-report.json` (consumed by the gate).
            "_lint-analyze" => {
                // Mirrors `build_lib`: a package with no Go source files isn't
                // analyzable. Tests/xtests are linted via their own variants
                // later; this is the normal-source unit.
                if pkg.go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(Arc::clone(&req.executor), &pkg, &[], &vref, &module_root)
                    .await
                    .map_err(GetError::Other)?;

                // One facts dep per first-party transitive lib: its `_lint-analyze` target
                // (same package + factors as its `build_lib`) produces the
                // `lint.facts` this package consumes for interprocedural analysis.
                //
                // A dep is only linted if ITS module opts in with a golangci
                // config, so a dep in a config-less module has no `_lint-analyze` target.
                // Skip those (they degrade to no-facts, like stdlib) rather than
                // wiring a dependency that would resolve to NotFound and break the
                // importer's lint — the same module-root gate the dep's own `_lint-analyze`
                // arm applies.
                let facts_libs: Vec<(String, Addr)> = transitive
                    .libs
                    .iter()
                    .filter(|(_, dep)| {
                        is_firstparty_pkg(dep.package.as_str())
                            && crate::plugingo::addr_util::find_go_mod(
                                &self.workspace_root.join(dep.package.as_str()),
                            )
                            .is_some_and(|(dep_module_root, _)| {
                                self.golangci_config_addr(&dep_module_root).is_some()
                            })
                    })
                    .map(|(ip, dep)| {
                        (
                            ip.clone(),
                            Addr::new(
                                dep.package.clone(),
                                "_lint-analyze".to_string(),
                                dep.args.clone(),
                            ),
                        )
                    })
                    .collect();

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;

                let govet_addr = self.govet_tool_addr().map_err(GetError::Other)?;

                // The module's `.golangci.yml`/`.golangci.yaml` (at the go.mod
                // root) drives linter selection AND opts the module into linting:
                // no config → no `_lint-analyze` target. The file is a hashed input, so a
                // config edit re-lints the module.
                let config_addr = match self.golangci_config_addr(&module_root) {
                    Some(a) => a,
                    None => return Err(GetError::NotFound),
                };
                let config_addr = Some(config_addr);

                let spec = crate::plugingo::driver_lint::build_lint_spec(
                    crate::plugingo::driver_lint::LintParams {
                        addr: addr.clone(),
                        import_path: &import_path,
                        factors: &factors,
                        go_version: &self.go_version,
                        transitive_libs: &transitive.libs,
                        facts_libs: &facts_libs,
                        src_addrs: &pkg_addrs.go_files,
                        govet_addr: &govet_addr,
                        config_addr: config_addr.as_ref(),
                    },
                );
                Ok(GetResponse { target_spec: spec })
            }
            "build" => {
                if pkg.name.as_deref() != Some("main") {
                    return Err(GetError::NotFound);
                }

                let own_lib_addr = self.build_lib_addr(addr, &vref);
                let mut transitive = Arc::clone(&self)
                    .collect_transitive_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &[],
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;
                transitive
                    .libs
                    .insert(0, (import_path.clone(), own_lib_addr));

                let link =
                    pick_link(&req.states, addr.package.as_str()).map_err(GetError::Other)?;
                let spec = target_bin::build_spec(
                    addr.clone(),
                    &import_path,
                    &factors,
                    &transitive.libs,
                    &link,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Generates testmain.go for the INTERNAL test bin (only `_test` imports).
            // Internal and external (xtest) testmains are emitted separately so each
            // test bin's importcfg is consistent: internal needs P=build_test_lib,
            // xtest needs P=build_lib (normal) — combining them in one bin is what
            // creates the fingerprint mismatch on cycle cases.
            "testmain" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                Ok(GetResponse {
                    target_spec: build_testmain_spec(
                        addr.clone(),
                        &golist_addr,
                        &pkg_addrs.test_go_files,
                        &[],
                    ),
                })
            }
            // Generates testmain.go for the EXTERNAL (xtest) test bin (only
            // `_xtest` imports). See `testmain` arm for the split rationale.
            "xtestmain" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                Ok(GetResponse {
                    target_spec: build_testmain_spec(
                        addr.clone(),
                        &golist_addr,
                        &[],
                        &pkg_addrs.xtest_go_files,
                    ),
                })
            }
            // Intermediate test target: compile GoFiles + TestGoFiles in test mode.
            "build_test_lib" => {
                let has_tests = !pkg.test_go_files.is_empty();
                let has_go = !pkg.go_files.is_empty();
                if !has_tests && !has_go {
                    return Err(GetError::NotFound);
                }
                let test_extra: Vec<String> = pkg.test_imports.clone();
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &test_extra,
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;

                let has_any_embed = !pkg.embed_patterns.is_empty()
                    || !pkg.embed_files.is_empty()
                    || !pkg.test_embed_patterns.is_empty()
                    || !pkg.test_embed_files.is_empty();
                let embed_golist = if has_any_embed {
                    Some(&golist_addr)
                } else {
                    None
                };

                let mut test_embed_files = pkg_addrs.embed_files.clone();
                test_embed_files.extend(pkg_addrs.test_embed_files.iter().cloned());
                let mut embed_src_addrs = if has_any_embed {
                    compute_embed_src_addrs(addr.package.as_str(), &req.states)
                        .map_err(GetError::Other)?
                } else {
                    Vec::new()
                };
                // Same poisoning guard as build_lib: if go list resolved zero
                // (test_)EmbedFiles for an embedding package, stage the static
                // non-go tree so the selector resolves the plain `//go:embed`
                // statics that a co-located go_embed_src pattern zeroed.
                if has_any_embed && test_embed_files.is_empty() {
                    embed_src_addrs.push(pkg_static_embed_glob_addr(addr.package.as_str()));
                }
                let spec = target_test::build_test_lib_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &transitive.libs,
                    &pkg_addrs.go_files,
                    &pkg_addrs.test_go_files,
                    embed_golist,
                    &test_embed_files,
                    &embed_src_addrs,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Intermediate test target: compile XTestGoFiles.
            "build_xtest_lib" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let xtest_imports_pkg = GoPackage {
                    import_path: format!("{}_test", import_path),
                    dir: pkg.dir.clone(),
                    name: pkg.name.clone(),
                    go_files: vec![],
                    s_files: vec![],
                    h_files: vec![],
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    test_embed_patterns: vec![],
                    test_embed_files: vec![],
                    xtest_embed_patterns: vec![],
                    xtest_embed_files: vec![],
                    imports: pkg.xtest_imports.clone(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
                // xtest (`package P_test`) imports P as a normal external import.
                // Normally xtest_lib's compile uses P=normal build_lib so its
                // embedded fingerprint matches the xtest bin's link-time view
                // (which also uses P=normal — xtest doesn't need P's test
                // variant). That keeps the xtest cycle (Q→P) consistent.
                //
                // Test-only flavors break the "P=normal" rule because P has no
                // GoFiles, so build_lib doesn't exist:
                //   - go_files empty, test_go_files non-empty (test+xtest-only)
                //     → use build_test_lib for P. No cycle is possible because
                //     nothing can import P normally.
                //   - both empty (xtest-only) → drop P from importcfg; xtest
                //     source can't reference symbols that don't exist.
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &xtest_imports_pkg,
                        &[],
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let p_lib_name = pick_xtest_p_lib_name(&pkg);
                let rewritten_libs: Vec<(String, Addr)> = transitive
                    .libs
                    .into_iter()
                    .filter_map(|(ip, a)| {
                        if ip != import_path {
                            return Some((ip, a));
                        }
                        p_lib_name
                            .map(|name| (ip, self.make_addr_with_name(&addr.package, name, &vref)))
                    })
                    .collect();

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;

                let xtest_embedding =
                    !pkg.xtest_embed_patterns.is_empty() || !pkg.xtest_embed_files.is_empty();
                let xtest_embed_golist = if xtest_embedding {
                    Some(&golist_addr)
                } else {
                    None
                };

                let embed_src_addrs = if xtest_embedding {
                    compute_embed_src_addrs(addr.package.as_str(), &req.states)
                        .map_err(GetError::Other)?
                } else {
                    Vec::new()
                };
                let spec = target_test::build_xtest_lib_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &rewritten_libs,
                    &pkg_addrs.xtest_go_files,
                    xtest_embed_golist,
                    &pkg_addrs.xtest_embed_files,
                    &embed_src_addrs,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Compile the INTERNAL testmain.go (imports `_test "P"` only).
            // Direct imports: testmain stdlib + P (via test_lib).
            "build_testmain_lib" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let testmain_imports = ["os", "reflect", "testing", "testing/internal/testdeps"];
                let testmain_pkg = make_testmain_pkg(&pkg, &testmain_imports);
                let mut transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &testmain_pkg,
                        &[],
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;
                // testmain imports `_test "P"` → importcfg needs P→test_lib.
                let test_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_test_lib", &vref);
                transitive.libs.push((import_path.clone(), test_lib_addr));

                let testmain_src_addr =
                    Addr::new(addr.package.clone(), "testmain".to_string(), vref.to_args());
                let spec = target_test::build_testmain_lib_spec(
                    addr.clone(),
                    &factors,
                    &testmain_src_addr,
                    &transitive.libs,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Compile the EXTERNAL (xtest) testmain.go (imports `_xtest "P_test"` only).
            // Direct imports: testmain stdlib + P_test (via xtest_lib).
            "build_xtestmain_lib" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let testmain_imports = ["os", "reflect", "testing", "testing/internal/testdeps"];
                let testmain_pkg = make_testmain_pkg(&pkg, &testmain_imports);
                let mut transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &testmain_pkg,
                        &[],
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;
                // testmain imports `_xtest "P_test"` → importcfg needs P_test→xtest_lib.
                let xtest_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_xtest_lib", &vref);
                transitive
                    .libs
                    .push((format!("{}_test", import_path), xtest_lib_addr));

                let testmain_src_addr = Addr::new(
                    addr.package.clone(),
                    "xtestmain".to_string(),
                    vref.to_args(),
                );
                let spec = target_test::build_testmain_lib_spec(
                    addr.clone(),
                    &factors,
                    &testmain_src_addr,
                    &transitive.libs,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Link the INTERNAL test bin.
            // importcfg: P=build_test_lib, transitive(P.imports ∪ P.test_imports)=build_lib.
            // Go rejects internal-test cycles, so no transitive importer of P appears here.
            "build_test" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let test_extra: Vec<String> = pkg
                    .test_imports
                    .iter()
                    .chain(
                        ["os", "reflect", "testing", "testing/internal/testdeps"]
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect::<Vec<_>>()
                            .iter(),
                    )
                    .cloned()
                    .collect();
                let transitive = Arc::clone(&self)
                    .collect_transitive_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &test_extra,
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                // Assemble flat importcfg list: dedup by importpath, then add P→test_lib.
                let mut all_libs: Vec<(String, Addr)> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                for (ip, a) in transitive.libs {
                    if ip == import_path {
                        continue; // P slot reserved for test_lib below
                    }
                    if seen.insert(ip.clone()) {
                        all_libs.push((ip, a));
                    }
                }
                let test_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_test_lib", &vref);
                all_libs.push((import_path.clone(), test_lib_addr));

                let testmain_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_testmain_lib", &vref);
                let spec = target_test::build_test_spec(
                    addr.clone(),
                    &factors,
                    &testmain_lib_addr,
                    &all_libs,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Link the EXTERNAL (xtest) test bin.
            // importcfg: P=build_lib (NORMAL — xtest_lib was compiled against
            // normal P too, so all consumers of P agree on the same .a),
            // P_test=build_xtest_lib, transitive(P.xtest_imports ∪ P.imports)=build_lib.
            // Allows xtest cycle (bsfilter→bsquery) because every reference to P
            // resolves to the SAME normal .a.
            "build_xtest" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let xtest_extra: Vec<String> = pkg
                    .xtest_imports
                    .iter()
                    .chain(&pkg.imports)
                    .chain(
                        ["os", "reflect", "testing", "testing/internal/testdeps"]
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect::<Vec<_>>()
                            .iter(),
                    )
                    .cloned()
                    .collect();
                // Walk from a synthetic pkg whose imports are xtest_extra; pkg.imports
                // are already in there, so the transitive closure covers everything
                // both P (normal) and P_test (xtest_lib) need.
                let xtest_root = GoPackage {
                    import_path: format!("{}_test", import_path),
                    dir: pkg.dir.clone(),
                    name: pkg.name.clone(),
                    go_files: vec![],
                    s_files: vec![],
                    h_files: vec![],
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    test_embed_patterns: vec![],
                    test_embed_files: vec![],
                    xtest_embed_patterns: vec![],
                    xtest_embed_files: vec![],
                    imports: xtest_extra.clone(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
                let transitive = Arc::clone(&self)
                    .collect_transitive_libs(
                        Arc::clone(&req.executor),
                        &xtest_root,
                        &[],
                        &vref,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let mut all_libs: Vec<(String, Addr)> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                // P's flavor in xtest bin must match xtest_lib's compile-time
                // view (see `pick_xtest_p_lib_name`). For pure xtest-only
                // packages P has no lib at all — skip the slot entirely so we
                // don't request a non-existent target.
                if let Some(p_lib_name) = pick_xtest_p_lib_name(&pkg) {
                    let p_addr = self.make_addr_with_name(&addr.package, p_lib_name, &vref);
                    all_libs.push((import_path.clone(), p_addr));
                    seen.insert(import_path.clone());
                } else {
                    // Still reserve the slot so a transitive resolution of P
                    // (resolves to build_lib addr that doesn't exist) doesn't
                    // sneak into the importcfg.
                    seen.insert(import_path.clone());
                }
                let p_test = format!("{}_test", import_path);
                seen.insert(p_test.clone()); // reserve for xtest_lib below
                for (ip, a) in transitive.libs {
                    if seen.insert(ip.clone()) {
                        all_libs.push((ip, a));
                    }
                }
                let xtest_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_xtest_lib", &vref);
                all_libs.push((p_test, xtest_lib_addr));

                let testmain_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_xtestmain_lib", &vref);
                let spec = target_test::build_test_spec(
                    addr.clone(),
                    &factors,
                    &testmain_lib_addr,
                    &all_libs,
                    &self.go_version,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Run the INTERNAL test bin.
            "test" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let build_test_addr = self.make_addr_with_name(&addr.package, "build_test", &vref);
                let data_query_addr = go_test_data_query_addr(addr.package.as_str());
                let test_env =
                    pick_test_env(&req.states, addr.package.as_str()).map_err(GetError::Other)?;
                let spec = target_test::test_spec(
                    addr.clone(),
                    build_test_addr,
                    &data_query_addr,
                    &test_env,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Run the EXTERNAL (xtest) test bin.
            "xtest" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let build_xtest_addr =
                    self.make_addr_with_name(&addr.package, "build_xtest", &vref);
                let data_query_addr = go_test_data_query_addr(addr.package.as_str());
                let test_env =
                    pick_test_env(&req.states, addr.package.as_str()).map_err(GetError::Other)?;
                let spec = target_test::test_spec(
                    addr.clone(),
                    build_xtest_addr,
                    &data_query_addr,
                    &test_env,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // `embed` / `embed_test` / `embed_xtest` targets are gone — the
            // `go_compile` driver resolves the embedcfg in-process.
            _ => Err(GetError::NotFound),
        }
    }

    /// Generate the `_golist` target spec without executing go list.
    fn get_golist_spec(
        &self,
        addr: Addr,
        kind: &GoPackageKind,
        factors: &Factors,
        states: &[State],
    ) -> anyhow::Result<GetResponse> {
        let spec = match kind {
            GoPackageKind::FirstParty {
                import_path,
                module_root,
                ..
            } => {
                let module_root_rel = module_root
                    .strip_prefix(&self.workspace_root)
                    .unwrap_or(module_root);
                let go_mod_addr = Addr::new(
                    hmodel::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
                    "_go_mod".to_string(),
                    Default::default(),
                );
                // Use a pluginfs glob directly instead of _go_src: _go_src spec
                // generation calls executor.result(_golist), which would deadlock.
                let pkg = addr.package.as_str();
                let src_glob = if pkg.is_empty() {
                    "*.go".to_string()
                } else {
                    format!("{}/*.go", pkg)
                };
                let go_src_glob_addr = pluginfs::glob_addr(&src_glob, &[]);
                // Non-Go src tree + go_src codegen query + go_codegen_deps — needed
                // so `go list` can resolve //go:embed patterns into EmbedFiles, and
                // shared with the downstream `embed` target.
                let extra_src_addrs = compute_pkg_src_addrs(pkg, states)?;
                target_golist::build_spec_firstparty(
                    addr,
                    import_path,
                    factors,
                    &self.go_version,
                    &go_mod_addr,
                    &go_src_glob_addr,
                    None,
                    &extra_src_addrs,
                )?
            }
            GoPackageKind::ThirdParty {
                module,
                version,
                subpath,
                module_root,
            } => {
                let import_path = if subpath.is_empty() {
                    module.clone()
                } else {
                    format!("{}/{}", module, subpath)
                };
                let module_root_rel = module_root
                    .strip_prefix(&self.workspace_root)
                    .unwrap_or(module_root);
                let go_mod_addr = Addr::new(
                    hmodel::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
                    "_go_mod".to_string(),
                    Default::default(),
                );
                let base_pkg = module_root_rel.to_string_lossy();
                let download_addr = encode_thirdparty_download(module, version, &base_pkg);
                target_golist::build_spec_thirdparty(
                    addr,
                    &import_path,
                    factors,
                    &self.go_version,
                    &go_mod_addr,
                    &download_addr,
                )?
            }
            GoPackageKind::Stdlib { import_path } => {
                target_golist::build_spec_stdlib(addr, import_path, factors, &self.go_version)?
            }
        };
        Ok(GetResponse { target_spec: spec })
    }

    fn get_stdlib(
        &self,
        addr: Addr,
        import_path: &str,
        factors: &Factors,
        vref: &VariantRef,
    ) -> Result<GetResponse, GetError> {
        match addr.name.as_str() {
            "build_lib" => {
                let spec = target_std::build_spec(addr, import_path, factors, vref);
                Ok(GetResponse { target_spec: spec })
            }
            _ => Err(GetError::NotFound),
        }
    }

    /// Read and parse the single package from a `_golist` target's output artifact.
    ///
    /// `executor.result(golist_addr)` is called OUTSIDE the `once` closure so
    /// every caller (owner + cache-hit waiters) routes through
    /// `Engine::result_addr`, which registers the `parent → golist_addr` edge
    /// in the request's `DepDag`. Memoizing the executor call inside the
    /// closure would let waiters skip dep registration, hiding a real target-
    /// dep cycle as a memoizer deadlock. The cache only memoizes the artifact
    /// parse, which is the expensive part.
    async fn read_golist_package(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        golist_addr: &Addr,
    ) -> anyhow::Result<Arc<GoPackage>> {
        // Fast path: the package is already parsed plugin-side, so we only need
        // to register the `parent → golist_addr` dep edge (the host's cycle
        // check). That's a cheap edge-only `note_dep` instead of a full
        // `result()`, which would re-run the engine's whole result pipeline plus
        // a lease round-trip for every edge — the dominant cost on the remote
        // resolve path (every transitive import hits this).
        if let Some(cached) = self.pkg_cache.peek(golist_addr) {
            executor.note_dep(golist_addr).await?;
            return cached.map_err(unwrap_arc_err);
        }
        let result = executor.result(golist_addr).await?;
        self.pkg_cache
            .once(
                golist_addr.clone(),
                enclose!((result) move || async move {
                    let pkg = hcore::blocking::run(move || -> anyhow::Result<_> {
                        for artifact in &result.artifacts {
                            for entry_result in artifact.walk()? {
                                let entry = entry_result?;
                                if entry.path.file_name().and_then(|n| n.to_str())
                                    != Some("package.bin")
                                {
                                    continue;
                                }
                                let data = match entry.kind {
                                    hcore::hartifactcontent::WalkEntryKind::File { data, .. } => data,
                                    hcore::hartifactcontent::WalkEntryKind::Symlink { .. } => continue,
                                };
                                return decode_go_package(data);
                            }
                        }
                        anyhow::bail!("_golist produced no package.bin")
                    })
                    .await?;
                    // `go list -e` reports no-buildable-files cases as a JSON
                    // entry with the Error field populated and empty GoFiles.
                    // Surface that as a typed sentinel so consumers can map it
                    // to NotFound uniformly (mirrors errNoGoFiles in the Go
                    // reference impl: heph/plugin/plugingo/pkg_analysis.go:34).
                    //
                    // Test-only packages (only `package pkg_test` xtest files)
                    // are NOT NOGO — go list -test reports a synthetic primary
                    // entry without an Error in that case, so xtest variants
                    // remain reachable.
                    if pkg.go_files.is_empty() && pkg.error.is_some() {
                        return Err(anyhow::Error::new(NoGoFilesError {
                            import_path: pkg.import_path.clone(),
                        }));
                    }
                    Ok(Arc::new(pkg))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn read_golist_package_addrs(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        golist_addr: &Addr,
    ) -> anyhow::Result<Arc<PackageAddrs>> {
        // executor.result is called outside the once closure so waiters register
        // the dep edge, not just the cache owner. Cache hit: cheap edge-only
        // note_dep (see read_golist_package).
        if let Some(cached) = self.pkg_addrs_cache.peek(golist_addr) {
            executor.note_dep(golist_addr).await?;
            return cached.map_err(unwrap_arc_err);
        }
        let result = executor.result(golist_addr).await?;
        self.pkg_addrs_cache
            .once(
                golist_addr.clone(),
                enclose!((result) move || async move {
                    let addrs = hcore::blocking::run(move || -> anyhow::Result<_> {
                        for artifact in &result.artifacts {
                            for entry_result in artifact.walk()? {
                                let entry = entry_result?;
                                if entry.path.file_name().and_then(|n| n.to_str())
                                    != Some("package_addrs.bin")
                                {
                                    continue;
                                }
                                let data = match entry.kind {
                                    hcore::hartifactcontent::WalkEntryKind::File { data, .. } => data,
                                    hcore::hartifactcontent::WalkEntryKind::Symlink { .. } => continue,
                                };
                                return decode_package_addrs(data);
                            }
                        }
                        anyhow::bail!("_golist produced no package_addrs.bin")
                    })
                    .await?;
                    Ok(Arc::new(addrs))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    fn build_lib_addr(&self, addr: &Addr, vref: &VariantRef) -> Addr {
        self.make_addr_with_name(&addr.package, "build_lib", vref)
    }

    /// Addr of the `heph-govet` binary the lint and format targets exec: the
    /// `govet` option (default: the release-download target, see [`govet`]).
    ///
    /// The tool runs natively on the build host — it analyzes code for any
    /// GOOS/GOARCH but is never itself cross-compiled — so an addr given without
    /// args is keyed by the *host's* factors, not the analyzed target's. That
    /// makes both flavors work unqualified: `//tools/heph-govet:build` builds for
    /// the host, and the download target's URL template renders the host's asset.
    /// An addr that carries its own args is taken verbatim.
    ///
    /// *Naming* the tool is not *resolving* it: a dev build's default addr points
    /// at a release that was never published, but that is diagnosed when the govet
    /// target itself is looked up (see the `GOVET_NAME` arm of `handle_get`) — not
    /// here. A lint/format spec must still resolve on a dev build, or a bulk
    /// `query` / `//...` walk (which asks every target for its spec) would die on
    /// a machine that has no business owning a heph-govet.
    fn govet_tool_addr(&self) -> anyhow::Result<Addr> {
        let addr = hmodel::htaddr::parse_addr(&self.govet).with_context(|| {
            format!(
                "go provider: `govet` must be a target addr (got {:?}) — omit it to download the \
                 released heph-govet, or point it at a build target like \
                 \"//tools/heph-govet:build\"",
                self.govet
            )
        })?;

        if !addr.args.is_empty() {
            return Ok(addr);
        }
        // The govet tool is a host-native binary, not variant-parameterized: its
        // `http_fetch` URL templates over plain `goos`/`goarch` args (like the Go
        // toolchain download), so it keeps that vocabulary rather than `v`/`vp`.
        let host_args = BTreeMap::from([
            ("goos".to_string(), current_goos()),
            ("goarch".to_string(), current_goarch()),
        ]);
        Ok(Addr::new(
            addr.package.clone(),
            addr.name.clone(),
            host_args,
        ))
    }

    fn make_addr_with_name(
        &self,
        package: &hmodel::htpkg::PkgBuf,
        name: &str,
        vref: &VariantRef,
    ) -> Addr {
        Addr::new(package.clone(), name.to_string(), vref.to_args())
    }

    /// Spec for the variant-free `format` / `format-check` targets. `Ok(None)`
    /// means "no such target here" (the caller maps it to `NotFound`).
    ///
    /// Formatting is syntactic, so this deliberately does **not** go through
    /// `_golist`: `go list` reports only the `GoFiles` the current
    /// `GOOS`/`GOARCH`/`-tags` select, which would leave every
    /// constraint-excluded file (`foo_windows.go` on a linux-only workspace) and
    /// every `_test.go` file permanently unformatted. Reading the package
    /// directory instead formats exactly what a developer sees in it.
    fn get_format(&self, addr: &Addr, kind: &GoPackageKind) -> anyhow::Result<Option<GetResponse>> {
        // `format` is listed for first-party packages only — stdlib/thirdparty
        // sources are vendored, not ours to rewrite.
        let GoPackageKind::FirstParty { module_root, .. } = kind else {
            return Ok(None);
        };

        // No variant to select. Reject args rather than ignore them, so a stale
        // `format@v=NAME` is an actionable error instead of silently doing
        // something else.
        if let Some(bad) = addr.args.keys().next() {
            anyhow::bail!(
                "unknown addr arg `{bad}` on go target `:{}` — formatting is \
                 syntactic and therefore variant-free; use a bare `:{}`",
                addr.name,
                addr.name,
            );
        }

        // Format only where the module opts in with a golangci config (the same
        // gate as lint); the config also carries formatter settings
        // (gofumpt/goimports).
        let Some(config_addr) = self.golangci_config_addr(module_root) else {
            return Ok(None);
        };

        let go_files = self.package_go_files_on_disk(addr.package.as_str())?;
        if go_files.is_empty() {
            return Ok(None);
        }
        let src_addrs: Vec<String> = go_files
            .iter()
            .map(|f| {
                let rel = if addr.package.as_str().is_empty() {
                    f.clone()
                } else {
                    format!("{}/{}", addr.package.as_str(), f)
                };
                pluginfs::file_addr(&rel).format()
            })
            .collect();

        let govet_addr = self.govet_tool_addr()?;
        let params = crate::plugingo::driver_format::FormatParams {
            addr: addr.clone(),
            govet_addr: &govet_addr,
            src_addrs: &src_addrs,
            go_files: &go_files,
            config_addr: Some(&config_addr),
        };
        let target_spec = if addr.name == "format" {
            crate::plugingo::driver_format::build_format_spec(params)
        } else {
            crate::plugingo::driver_format::build_format_check_spec(params)
        };
        Ok(Some(GetResponse { target_spec }))
    }

    /// Every `.go` file directly in `pkg`'s directory, sorted, as basenames.
    ///
    /// Build constraints are deliberately not applied — see [`Self::get_format`].
    /// Codegen-stamped files are skipped: they are owned by their generator (an
    /// `fs:file` over one resolves to nothing anyway), and declaring one as a
    /// `codegen = in_place` output of `format` would collide with the generator's
    /// own output. Files reached through a `.heph*` cache dir are skipped for the
    /// same reason they are in the fs provider — they are engine artifacts, not
    /// source.
    fn package_go_files_on_disk(&self, pkg: &str) -> anyhow::Result<Vec<String>> {
        let dir = if pkg.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(pkg)
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow::Error::new(e).context(format!("read go pkg dir {dir:?}"))),
        };
        let mut files: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("read go pkg dir entry in {dir:?}"))?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".go") {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            if pluginfs::has_codegen_xattr(&path) || pluginfs::resolves_into_heph_dir(&path) {
                continue;
            }
            files.push(name.to_string());
        }
        // `read_dir` order is filesystem-defined; sort so the spec (and therefore
        // the target's input hash) is reproducible.
        files.sort();
        Ok(files)
    }

    /// Spec for the user-facing `lint-check` (gate) and `lint` (fixer) targets:
    /// one bare addr per package that aggregates the per-variant `_lint-analyze`
    /// units of every module-bounded ancestry variant.
    ///
    /// Why aggregate rather than let the user select a variant (see
    /// [`VARIANT_AGGREGATE_TARGET_NAMES`]): lint rules don't depend on the
    /// variant, only the typed package they run against does. Selecting one
    /// variant therefore reported on an arbitrary subset of the package's files
    /// — `foo_windows.go` was silently unlinted on a host-variant run — and gave
    /// every variant its own fixer, each declaring the same source files as
    /// `codegen = in_place` outputs.
    ///
    /// A variant whose build constraints leave the package with no Go files is
    /// skipped: it has no `_lint-analyze` target, so wiring one would be a dep
    /// that resolves to `NotFound`. All variants skipped (or none declared) →
    /// `NotFound`, matching what the per-variant targets did.
    async fn get_lint_aggregate(
        self: Arc<Self>,
        req: &GetRequest,
        kind: &GoPackageKind,
        module_root: &str,
    ) -> Result<GetResponse, GetError> {
        let addr = &req.addr;

        // Linting is first-party only: std/thirdparty sources are vendored, and
        // their modules carry no golangci config to opt in with.
        let GoPackageKind::FirstParty {
            module_root: module_root_path,
            ..
        } = kind
        else {
            return Err(GetError::NotFound);
        };

        // No variant to select any more. Reject a stale `@v=`/`@vp=` rather than
        // ignore it, so a pinned addr surfaces the migration instead of quietly
        // linting a different (now wider) set of files.
        if let Some(bad) = addr.args.keys().next() {
            return Err(GetError::Other(anyhow::anyhow!(
                "unknown addr arg `{bad}` on go target `:{}` — lint rules are \
                 variant-independent, so `:{}` now aggregates every declared \
                 variant's analysis; use a bare `:{}` (the per-variant unit is \
                 `:_lint-analyze@v=NAME,vp=PKG`)",
                addr.name,
                addr.name,
                addr.name,
            )));
        }

        // Lint only where the module opts in with a golangci config; attach it as
        // a hash-only dep so a config change re-keys the gate/fixer directly.
        let Some(config_addr) = self.golangci_config_addr(module_root_path) else {
            return Err(GetError::NotFound);
        };

        // The ancestry variants — the same set `list` used to multiply these
        // targets across, now folded into one target's deps.
        let vrefs: Vec<VariantRef> =
            variant::ancestry_variants_with_factors(&req.states, module_root)
                .into_iter()
                .map(|(v, _)| v)
                .collect();

        // One `_golist` read per variant, fanned out: they are independent and
        // engine-cached (the `_lint-analyze` targets read the very same ones).
        let per_variant = futures::future::try_join_all(vrefs.iter().map(|vref| {
            let this = Arc::clone(&self);
            let executor = Arc::clone(&req.executor);
            let golist_addr = self.make_addr_with_name(&addr.package, "_golist", vref);
            async move {
                match this.read_golist_package(executor, &golist_addr).await {
                    // No buildable Go files under this variant's constraints →
                    // no analysis unit to aggregate.
                    Ok(pkg) if pkg.go_files.is_empty() => anyhow::Ok(None),
                    Ok(pkg) => anyhow::Ok(Some((vref.clone(), pkg, golist_addr))),
                    Err(e) if downcast_chain_ref::<NoGoFilesError>(&e).is_some() => {
                        anyhow::Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
        }))
        .await
        .map_err(GetError::Other)?;

        let analyzed: Vec<(VariantRef, Arc<GoPackage>, Addr)> =
            per_variant.into_iter().flatten().collect();
        if analyzed.is_empty() {
            return Err(GetError::NotFound);
        }

        let analyze_addrs: Vec<Addr> = analyzed
            .iter()
            .map(|(vref, _, _)| self.make_addr_with_name(&addr.package, "_lint-analyze", vref))
            .collect();

        if addr.name == "lint-check" {
            let spec = crate::plugingo::driver_lint::build_lint_gate_spec(
                addr.clone(),
                &analyze_addrs,
                Some(&config_addr),
            );
            return Ok(GetResponse { target_spec: spec });
        }

        // The fixer additionally stages the sources it rewrites. Union them
        // across variants: a file the selected variant's constraints excluded is
        // still linted (and so still fixable) under another one, and it must be a
        // declared output or the engine has nowhere to write the fix back.
        let addrs_per_variant =
            futures::future::try_join_all(analyzed.iter().map(|(_, _, golist_addr)| {
                self.read_golist_package_addrs(Arc::clone(&req.executor), golist_addr)
            }))
            .await
            .map_err(GetError::Other)?;

        // Keyed by basename (a Go package's files are flat, so basenames are
        // unique in it) → source addr. `resolve_package_addrs` maps files 1:1 in
        // order, so `go_files[i]` is the addr of `pkg.go_files[i]`. `BTreeMap`
        // keeps the two emitted lists aligned, deduped and in a stable order, so
        // the spec — and the target's input hash — is reproducible.
        let mut by_file: BTreeMap<String, String> = BTreeMap::new();
        for ((_, pkg, _), pkg_addrs) in analyzed.iter().zip(addrs_per_variant.iter()) {
            for (file, src) in pkg.go_files.iter().zip(pkg_addrs.go_files.iter()) {
                by_file.entry(file.clone()).or_insert_with(|| src.clone());
            }
        }
        let go_files: Vec<String> = by_file.keys().cloned().collect();
        let src_addrs: Vec<String> = by_file.into_values().collect();

        let spec = crate::plugingo::driver_lint::build_lint_fix_spec(
            addr.clone(),
            &analyze_addrs,
            &src_addrs,
            &go_files,
            Some(&config_addr),
        );
        Ok(GetResponse { target_spec: spec })
    }

    /// The addr of the module's golangci-lint config, if the module root (the
    /// `go.mod` directory) holds one. Lint/format targets exist ONLY for modules
    /// that have such a config — the presence of a `.golangci.yml`/`.golangci.yaml`
    /// at the module root is what opts a module into linting. The returned addr
    /// (a workspace-relative `fs:file`) is a hashed input to the lint/format
    /// targets, so editing the config re-lints the module.
    fn golangci_config_addr(&self, module_root: &Path) -> Option<Addr> {
        for name in [".golangci.yml", ".golangci.yaml"] {
            let abs = module_root.join(name);
            if abs.exists() {
                let rel = abs.strip_prefix(&self.workspace_root).unwrap_or(&abs);
                return Some(pluginfs::file_addr(&rel.to_string_lossy()));
            }
        }
        None
    }

    /// Build the cache key for `collect_*_libs`. Imports are sorted+deduped so
    /// distinct caller-side orderings of the same logical input set hash to one entry.
    fn libs_key(
        root_pkg: &GoPackage,
        extra_imports: &[String],
        vref: &VariantRef,
        module_root: &Path,
        transitive: bool,
    ) -> LibsKey {
        let mut imports: Vec<String> = root_pkg.imports.clone();
        imports.sort();
        imports.dedup();
        let mut extra: Vec<String> = extra_imports.to_vec();
        extra.sort();
        extra.dedup();
        LibsKey {
            imports,
            extra,
            vref: vref.clone(),
            module_root: module_root.to_path_buf(),
            transitive,
        }
    }

    /// Collect all transitive lib addresses for a package's imports, recursively.
    ///
    /// Each BFS frontier is processed concurrently via `try_join_all`. Each dep's
    /// `_golist` target is fetched via `executor.result` (engine pipeline with disk
    /// cache), memoized in `pkg_cache` for the Provider lifetime so repeated calls
    /// for the same dep cost nothing after the first resolution.
    ///
    /// The full BFS result is itself memoized via `libs_cache`, deduping calls
    /// across `build_lib`/`build`/`build_test`/`build_test_lib` etc. for the same
    /// root pkg + factors within a single Provider lifetime.
    async fn collect_transitive_libs(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        vref: &VariantRef,
        module_root: &Path,
    ) -> anyhow::Result<TransitiveDeps> {
        self.collect_libs(executor, root_pkg, extra_imports, vref, module_root, true)
            .await
    }

    /// Resolve direct imports only (no recursion) — correct for compile steps.
    async fn collect_direct_libs(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        vref: &VariantRef,
        module_root: &Path,
    ) -> anyhow::Result<TransitiveDeps> {
        self.collect_libs(executor, root_pkg, extra_imports, vref, module_root, false)
            .await
    }

    async fn collect_libs(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        vref: &VariantRef,
        module_root: &Path,
        transitive: bool,
    ) -> anyhow::Result<TransitiveDeps> {
        let key = Self::libs_key(root_pkg, extra_imports, vref, module_root, transitive);
        let extra = extra_imports.to_vec();
        let module_root = module_root.to_path_buf();
        let arc = self
            .libs_cache
            .once(
                key,
                enclose!((self => me, executor, vref, root_pkg.imports => root_imports) move || async move {
                    me.collect_libs_inner(
                        executor,
                        &root_imports,
                        &extra,
                        &vref,
                        &module_root,
                        transitive,
                    )
                    .await
                    .map(Arc::new)
                }),
            )
            .await
            .map_err(unwrap_arc_err)?;
        Ok((*arc).clone())
    }

    fn load_go_mod(&self, module_root: &Path) -> anyhow::Result<Arc<GoModData>> {
        let go_mod_path = module_root.join("go.mod");
        if let Some(hit) = self.go_mod_cache.read().get(&go_mod_path) {
            return Ok(Arc::clone(hit));
        }
        let data = if go_mod_path.exists() {
            // Stays inline: one small `read_to_string`, from a sync fn, memoized
            // per module. Not worth an async hop onto the blocking pool.
            let content = hproc::process_supervisor::block_or_inline(
                enclose!((go_mod_path) move || std::fs::read_to_string(&go_mod_path)),
            )
            .with_context(|| format!("reading {}", go_mod_path.display()))?;

            // Start from go.mod's explicit requires, then fill in modules that
            // appear only in go.sum. An untidied go.mod (pre-1.17 / never
            // tidied) omits indirect requires, so a thirdparty package's
            // transitive imports (e.g. golang.org/x/net behind oauth2) would
            // otherwise fail to resolve to a module version and get dropped from
            // the importcfg. go.mod entries win on conflict — they carry the
            // authoritative/replace intent — so go.sum only adds modules go.mod
            // doesn't already pin.
            let mut requires = parse_go_mod_requires(&content);
            let go_sum_path = module_root.join("go.sum");
            if go_sum_path.exists() {
                let sum_content = hproc::process_supervisor::block_or_inline(
                    enclose!((go_sum_path) move || std::fs::read_to_string(&go_sum_path)),
                )
                .with_context(|| format!("reading {}", go_sum_path.display()))?;
                let known: std::collections::HashSet<&str> =
                    requires.iter().map(|(m, _)| m.as_str()).collect();
                let extra: Vec<(String, String)> = parse_go_sum_modules(&sum_content)
                    .into_iter()
                    .filter(|(m, _)| !known.contains(m.as_str()))
                    .collect();
                requires.extend(extra);
            }

            Arc::new(GoModData {
                requires,
                module_path: parse_go_mod_module_path(&content).unwrap_or_default(),
            })
        } else {
            Arc::new(GoModData {
                requires: Vec::new(),
                module_path: String::new(),
            })
        };
        let mut w = self.go_mod_cache.write();
        Ok(Arc::clone(
            w.entry(go_mod_path).or_insert_with(|| Arc::clone(&data)),
        ))
    }

    /// Memoized per-import_path transitive closure.
    ///
    /// Returns `import_path`'s lib (if any) plus the transitive closure of its
    /// sub-imports, in deps-first order with import_paths deduped. Cached by
    /// `(import_path, factors, module_root)` — so a hot dep like `fmt` is walked
    /// once per request even if hundreds of top-level targets reach it.
    ///
    /// Recurses via `try_join_all` over each sub-import; each recursive call
    /// hits the same cache, so the work for any subtree is amortized.
    #[async_recursion]
    async fn import_closure(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        import_path: String,
        vref: VariantRef,
        go_mod: Arc<GoModData>,
        module_root: PathBuf,
    ) -> anyhow::Result<Arc<ImportClosure>> {
        let key = ImportClosureKey {
            import_path: import_path.clone(),
            vref: vref.clone(),
            module_root: module_root.clone(),
        };
        self.import_closure_cache
            .once(
                key,
                enclose!((self => me, executor, import_path, vref, go_mod, module_root) move || async move {
                    let (resolved_path, dep_addr_opt, sub_imports) = me
                        .resolve_import(
                            Arc::clone(&executor),
                            &import_path,
                            &vref,
                            &go_mod.requires,
                            &go_mod.module_path,
                            &module_root,
                        )
                        .await?;

                    let sub_closures = try_join_all(
                        sub_imports
                            .into_iter()
                            .filter(|s| s != "unsafe" && s != "C")
                            .map(|sub| {
                                Arc::clone(&me).import_closure(
                                    Arc::clone(&executor),
                                    sub,
                                    vref.clone(),
                                    Arc::clone(&go_mod),
                                    module_root.clone(),
                                )
                            }),
                    )
                    .await?;

                    // Deps first, then self (deduped by import_path so a diamond
                    // dependency only shows once).
                    let self_lib =
                        dep_addr_opt.map(|addr| (Arc::<str>::from(resolved_path), addr));
                    let libs = compose_closures(&sub_closures, self_lib);

                    anyhow::Ok(Arc::new(ImportClosure { libs }))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn collect_libs_inner(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_imports: &[String],
        extra_imports: &[String],
        vref: &VariantRef,
        module_root: &Path,
        transitive: bool,
    ) -> anyhow::Result<TransitiveDeps> {
        let go_mod = self.load_go_mod(module_root)?;

        if transitive {
            // Pre-dedupe the top-level set so we don't fan out the same
            // import_path twice (also halves cache lookups for repeated entries
            // between `root_imports` and `extra_imports`).
            let mut unique_imports: Vec<String> = root_imports
                .iter()
                .chain(extra_imports.iter())
                .filter(|i| *i != "unsafe" && *i != "C")
                .cloned()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            // HashSet iteration order is randomized per process; sort so the
            // resulting transitive lib order is deterministic run-to-run.
            unique_imports.sort();

            let module_root_buf = module_root.to_path_buf();
            let sub_closures = try_join_all(unique_imports.into_iter().map(|ip| {
                Arc::clone(&self).import_closure(
                    Arc::clone(&executor),
                    ip,
                    vref.clone(),
                    Arc::clone(&go_mod),
                    module_root_buf.clone(),
                )
            }))
            .await?;

            // Compose the root sub-closures into one deps-first deduped set, then
            // materialize owned `String`s once for `TransitiveDeps` — O(closure)
            // String allocations total rather than O(closure) per node.
            let libs = compose_closures(&sub_closures, None)
                .into_iter()
                .map(|(ip, addr)| (ip.to_string(), addr))
                .collect();
            return Ok(TransitiveDeps { libs });
        }

        let go_mod_requires = &go_mod.requires;
        let workspace_module_path = go_mod.module_path.as_str();

        let mut imports: Vec<String> = root_imports
            .iter()
            .chain(extra_imports.iter())
            .filter(|i| *i != "unsafe" && *i != "C")
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        // HashSet iteration order is randomized per process; sort so the
        // resulting `libs` order is deterministic run-to-run (mirrors the
        // transitive branch above). The go_compile cache key sorts at the hash
        // boundary too, but keeping this lane deterministic avoids surprising any
        // other consumer of `TransitiveDeps.libs`.
        imports.sort();

        let results = try_join_all(imports.iter().map(|ip| {
            self.resolve_import(
                Arc::clone(&executor),
                ip,
                vref,
                go_mod_requires,
                workspace_module_path,
                module_root,
            )
        }))
        .await?;

        let libs = results
            .into_iter()
            .filter_map(|(import_path, addr_opt, _sub)| addr_opt.map(|a| (import_path, a)))
            .collect();

        Ok(TransitiveDeps { libs })
    }

    /// Resolve one import path: returns `(import_path, Option<lib Addr>, sub_imports)`.
    async fn resolve_import(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        import_path: &str,
        vref: &VariantRef,
        go_mod_requires: &[(String, String)],
        workspace_module_path: &str,
        module_root: &Path,
    ) -> anyhow::Result<(String, Option<Addr>, Vec<String>)> {
        let is_workspace_module = !workspace_module_path.is_empty()
            && (import_path == workspace_module_path
                || import_path.starts_with(&format!("{}/", workspace_module_path)));
        if !is_workspace_module && is_stdlib_import_path(import_path) {
            let addr = encode_stdlib(import_path, vref);
            let golist_addr = Addr::new(
                hmodel::htpkg::PkgBuf::from(format!("@heph/go/std/{}", import_path)),
                "_golist".to_string(),
                vref.to_args(),
            );
            // Propagate golist errors instead of swallowing them: a missing or
            // partial closure here turns into a broken link step downstream
            // ("cannot find package errors (using -importcfg)") that's
            // impossible to root-cause from the user's side.
            let sub_imports = self
                .read_golist_package(Arc::clone(&executor), &golist_addr)
                .await
                .map(|pkg| pkg.imports.clone())
                .with_context(|| format!("read _golist for stdlib {}", import_path))?;
            return Ok((import_path.to_string(), Some(addr), sub_imports));
        }

        let dep_addr = match self.resolve_import_to_addr(
            import_path,
            vref,
            module_root,
            workspace_module_path,
            go_mod_requires,
        ) {
            Some(a) => a,
            None => return Ok((import_path.to_string(), None, vec![])),
        };

        let golist_addr = Addr::new(
            dep_addr.package.clone(),
            "_golist".to_string(),
            dep_addr.args.clone(),
        );

        let sub_imports = match self.read_golist_package(executor, &golist_addr).await {
            Ok(p) => p.imports.clone(),
            // The dep's directory has Go files but none are buildable for these
            // factors (all excluded by build constraints). Its `build_lib`
            // resolves to NotFound (see the NoGoFilesError arm in `handle_get`),
            // so there's no lib to link against — drop the import instead of
            // failing the importer's get_spec, exactly as an unresolvable import
            // returns `None` above.
            Err(e) if downcast_chain_ref::<NoGoFilesError>(&e).is_some() => {
                return Ok((import_path.to_string(), None, vec![]));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("read _golist for {}", import_path));
            }
        };

        Ok((import_path.to_string(), Some(dep_addr), sub_imports))
    }

    /// Resolve an import path to a heph `build_lib` Addr.
    fn resolve_import_to_addr(
        &self,
        import_path: &str,
        vref: &VariantRef,
        module_root: &Path,
        workspace_module_path: &str,
        go_mod_requires: &[(String, String)],
    ) -> Option<Addr> {
        // Check if it's a first-party import (in the workspace module).
        // We use workspace_module_path (from go.mod) rather than root_pkg.module.path
        // because root_pkg may itself be a third-party package — its sub-packages must
        // still be resolved as third-party, not mapped into the workspace.
        if !workspace_module_path.is_empty()
            && (import_path == workspace_module_path
                || import_path.starts_with(&format!("{}/", workspace_module_path)))
        {
            let rel_suffix = import_path
                .strip_prefix(workspace_module_path)
                .and_then(|s| s.strip_prefix('/'))
                .unwrap_or("");
            let module_rel = module_root
                .strip_prefix(&self.workspace_root)
                .unwrap_or(module_root);
            let src_dir = if rel_suffix.is_empty() {
                self.workspace_root.join(module_rel)
            } else {
                self.workspace_root.join(module_rel).join(rel_suffix)
            };
            return Some(encode_firstparty(&src_dir, &self.workspace_root, vref));
        }

        // Third-party: look up in go.mod requires
        if let Some((mod_path, version)) = find_module_for_import(import_path, go_mod_requires) {
            let subpath = import_path
                .strip_prefix(&mod_path)
                .and_then(|s| s.strip_prefix('/'))
                .unwrap_or("")
                .to_string();
            let base_pkg = module_root
                .strip_prefix(&self.workspace_root)
                .unwrap_or_else(|_| Path::new(""))
                .to_string_lossy()
                .to_string();
            return Some(encode_thirdparty(
                &mod_path, &version, &subpath, &base_pkg, vref,
            ));
        }

        None
    }
}

/// Build a synthetic `GoPackage` representing the `main` package of a
/// testmain.go file. Only `imports` is set — used to drive
/// `collect_direct_libs` resolution.
fn make_testmain_pkg(pkg: &GoPackage, imports: &[&str]) -> GoPackage {
    GoPackage {
        import_path: "main".to_string(),
        dir: pkg.dir.clone(),
        name: Some("main".to_string()),
        go_files: vec![],
        s_files: vec![],
        h_files: vec![],
        test_go_files: vec![],
        xtest_go_files: vec![],
        embed_patterns: vec![],
        embed_files: vec![],
        test_embed_patterns: vec![],
        test_embed_files: vec![],
        xtest_embed_patterns: vec![],
        xtest_embed_files: vec![],
        imports: imports.iter().map(|s| (*s).to_string()).collect(),
        test_imports: vec![],
        xtest_imports: vec![],
        standard: false,
        module: pkg.module.clone(),
        match_: vec![],
        incomplete: false,
        error: None,
    }
}

fn build_testmain_spec(
    addr: Addr,
    golist_addr: &Addr,
    test_file_addrs: &[String],
    xtest_file_addrs: &[String],
) -> hplugin::provider::TargetSpec {
    use hcore::htvalue::Value;
    use std::collections::HashMap;

    let golist_dep = format!("{}|pkg", golist_addr.format());

    let mut deps_map: HashMap<String, Value> = HashMap::new();
    deps_map.insert(
        "golist".to_string(),
        Value::List(vec![Value::String(golist_dep)]),
    );
    if !test_file_addrs.is_empty() {
        deps_map.insert(
            "test".to_string(),
            Value::List(
                test_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !xtest_file_addrs.is_empty() {
        deps_map.insert(
            "xtest".to_string(),
            Value::List(
                xtest_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }

    // Tell the `go_testmain` driver which file set to analyze. Without `mode`
    // it analyzes BOTH test_go_files and xtest_go_files from the golist, which
    // breaks the split-bin design (xtestmain tries to open internal test files
    // that weren't staged → "No such file or directory").
    let mode = if !test_file_addrs.is_empty() && !xtest_file_addrs.is_empty() {
        "both"
    } else if !xtest_file_addrs.is_empty() {
        "xtest"
    } else {
        "internal"
    };

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("deps".to_string(), Value::Map(deps_map));
    config.insert("mode".to_string(), Value::String(mode.to_string()));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "go".to_string(),
            Value::List(vec![Value::String("testmain.go".to_string())]),
        )])),
    );

    hplugin::provider::TargetSpec {
        addr,
        driver: "go_testmain".to_string(),
        config,
        ..Default::default()
    }
}

use hplugin::provider::{ProbeRequest, ProbeResponse};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugingo::addr_util::decode_package;
    use crate::plugingo::factors::Factors;
    use crate::plugingo::pkg_analysis::run_go_list;
    use anyhow::Context;
    use futures::future::BoxFuture;
    use hcore::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htpkg::PkgBuf;
    use hplugin::eresult::{ArtifactMeta, EResult};
    use hplugin::provider::{GetError, GetRequest, Provider as ProviderTrait};
    use std::collections::HashMap;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn build_addr_ctx() -> FnCallContext<'static> {
        FnCallContext {
            pkg: "",
            root: std::path::Path::new("/"),
        }
    }

    #[tokio::test]
    async fn test_build_addr_basic() {
        let args = FnArgs {
            positional: vec![
                Value::String("mylib".into()),
                Value::String("release".into()),
            ],
            named: HashMap::new(),
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(v, Value::String("//mylib:build@v=release".into()));
    }

    #[tokio::test]
    async fn test_build_addr_named_args() {
        let mut named = HashMap::new();
        named.insert("pkg".to_string(), Value::String("mylib".into()));
        named.insert("variant".to_string(), Value::String("release".into()));
        let args = FnArgs {
            positional: vec![],
            named,
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(v, Value::String("//mylib:build@v=release".into()));
    }

    // Omitting `variant` returns the magic host-default `build` addr (bare, no
    // `@v`) — the provider serves it as a `group` to the host-matching variant.
    #[tokio::test]
    async fn test_build_addr_without_variant_returns_magic_addr() {
        let args = FnArgs {
            positional: vec![Value::String("mylib".into())],
            named: HashMap::new(),
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(v, Value::String("//mylib:build".into()));
    }

    // An explicit empty variant is treated the same as omitting it.
    #[tokio::test]
    async fn test_build_addr_empty_variant_returns_magic_addr() {
        let args = FnArgs {
            positional: vec![Value::String("mylib".into()), Value::String("".into())],
            named: HashMap::new(),
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(v, Value::String("//mylib:build".into()));
    }

    fn go_available() -> bool {
        std::process::Command::new("go")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    macro_rules! require_go {
        () => {
            if !go_available() {
                eprintln!("skipping: go not in PATH");
                return;
            }
        };
    }

    // --- Test executor that resolves _golist by running go list directly ---

    struct GoListTestExecutor {
        workspace_root: PathBuf,
        /// Source map applied when generating `package_addrs.bin`.
        source_map: HashMap<String, String>,
        /// The variant declarations this workspace has. Served from
        /// `states_under`, and used to turn an addr's `v` into the `GOOS`/`GOARCH`
        /// the mocked `go list` runs under — so a `_golist` really does see a
        /// different file set per variant, as it does in production.
        states: Vec<State>,
    }

    struct BinaryArtifact {
        path: PathBuf,
        bytes: Vec<u8>,
        hashout: String,
    }

    impl Content for BinaryArtifact {
        fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
            Ok(Box::new(io::Cursor::new(self.bytes.clone())))
        }

        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            let entry = WalkEntry {
                path: self.path.clone(),
                kind: WalkEntryKind::File {
                    data: Box::new(io::Cursor::new(self.bytes.clone())),
                    x: false,
                },
            };
            Ok(Box::new(std::iter::once(Ok(entry))))
        }

        fn hashout(&self) -> anyhow::Result<String> {
            Ok(self.hashout.clone())
        }
    }

    impl ProviderExecutor for GoListTestExecutor {
        // The test fixtures are single-module workspaces (go.mod at root), so the
        // module universe = the variants declared at the root. Serve them for
        // `states_under` of the root prefix; deeper prefixes have no declarations.
        fn states_under<'a>(
            &'a self,
            prefix: &'a hmodel::htpkg::PkgBuf,
        ) -> BoxFuture<'a, anyhow::Result<Vec<hplugin::provider::State>>> {
            let states = if prefix.as_str().is_empty() {
                self.states.clone()
            } else {
                vec![]
            };
            Box::pin(async move { Ok(states) })
        }

        fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
            Box::pin(async move {
                if addr.name != "_golist" {
                    anyhow::bail!(
                        "GoListTestExecutor: only handles _golist, got: {}",
                        addr.format()
                    );
                }

                // Resolve the addr's variant against the declared states so the
                // mocked `go list` runs with that variant's `GOOS`/`GOARCH` —
                // build constraints must select a different file set per variant
                // here just as they do in production. Falls back to host factors
                // for an addr with no `v` (or a name this workspace never
                // declared).
                let factors = addr
                    .args
                    .get("v")
                    .and_then(|name| variant::resolve_ancestry(name, &self.states, "").ok())
                    .map_or_else(
                        || Factors {
                            goos: current_goos(),
                            goarch: current_goarch(),
                            ..Default::default()
                        },
                        |(f, _)| f,
                    );
                let kind = decode_package(&addr.package, &self.workspace_root)
                    .ok_or_else(|| anyhow::anyhow!("unknown package: {}", addr.package))?;

                let (import_path, run_dir) = match &*kind {
                    GoPackageKind::FirstParty {
                        import_path,
                        module_root,
                        ..
                    } => (import_path.clone(), module_root.clone()),
                    GoPackageKind::ThirdParty {
                        module,
                        subpath,
                        module_root,
                        ..
                    } => {
                        let ip = if subpath.is_empty() {
                            module.clone()
                        } else {
                            format!("{}/{}", module, subpath)
                        };
                        (ip, module_root.clone())
                    }
                    GoPackageKind::Stdlib { import_path } => {
                        // Stdlib packages don't need a module root; run go list from workspace.
                        (import_path.clone(), self.workspace_root.clone())
                    }
                };

                // Run go list (no -deps) for just this package
                let packages = run_go_list(&import_path, &factors, &run_dir).await?;
                // `-test` returns multiple variants; pick the canonical entry whose
                // ImportPath matches the request.
                let pkg = packages.get(&import_path).cloned().ok_or_else(|| {
                    anyhow::anyhow!("go list returned no entry for {}", import_path)
                })?;
                let pkg_bin = crate::plugingo::pkg_analysis::encode_go_package(&pkg)
                    .context("encode package.bin")?;

                // Mirror the real driver: also emit package_addrs.bin so the provider
                // can resolve per-file addrs without re-running the driver.
                let addrs = crate::plugingo::pkg_analysis::resolve_package_addrs(
                    &pkg,
                    addr.package.as_str(),
                    &self.source_map,
                    None,
                );
                let addrs_bin = crate::plugingo::pkg_analysis::encode_package_addrs(&addrs)
                    .context("encode package_addrs.bin")?;

                let artifacts: Vec<Arc<dyn Content>> = vec![
                    Arc::new(BinaryArtifact {
                        path: PathBuf::from("package.bin"),
                        bytes: pkg_bin,
                        hashout: "test_hashout".to_string(),
                    }) as Arc<dyn Content>,
                    Arc::new(BinaryArtifact {
                        path: PathBuf::from("package_addrs.bin"),
                        bytes: addrs_bin,
                        hashout: "test_hashout_addrs".to_string(),
                    }) as Arc<dyn Content>,
                ];

                Ok(Arc::new(EResult {
                    artifacts_meta: artifacts
                        .iter()
                        .map(|_| ArtifactMeta {
                            hashout: "test_hashout".to_string(),
                        })
                        .collect(),
                    artifacts,
                    support_artifacts: vec![],
                }))
            })
        }

        fn query<'a>(
            &'a self,
            _m: &'a hmodel::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    fn test_executor(workspace_root: &std::path::Path) -> Arc<dyn ProviderExecutor> {
        Arc::new(GoListTestExecutor {
            workspace_root: workspace_root.to_path_buf(),
            source_map: HashMap::new(),
            states: vec![host_variant_state()],
        })
    }

    /// Copy a testdata fixture directory to a fresh tempdir sandbox.
    fn copy_fixture(name: &str) -> tempfile::TempDir {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/plugingo/testdata")
            .join(name);
        let tmp = tempfile::tempdir().unwrap();
        copy_dir_all(&src, tmp.path()).unwrap();
        tmp
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
            } else {
                std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    /// Build a fully-resolved internal addr carrying the default test variant
    /// (`host`, defined at the root by [`host_variant_state`]).
    fn make_addr(package: &str, name: &str) -> Addr {
        Addr::new(
            PkgBuf::from(package),
            name.to_string(),
            VariantRef::new("host", "").to_args(),
        )
    }

    /// Addr with no variant args — for the variant-free targets
    /// (`format`/`format-check`, `download`, `_go_mod`).
    fn make_bare_addr(package: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(package), name.to_string(), Default::default())
    }

    /// A root `provider_state(provider="go", variants={"host": {goos, goarch}})`
    /// defining the default `host` variant every test target resolves against.
    fn host_variant_state() -> State {
        let variant = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String(current_goos())),
            ("goarch".to_string(), Value::String(current_goarch())),
        ]));
        State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: HashMap::from([(
                "variants".to_string(),
                Value::Map(HashMap::from([("host".to_string(), variant)])),
            )]),
        }
    }

    /// Write a minimal `.golangci.yml` at a fixture's module root so its lint and
    /// format targets are enabled — the provider synthesizes them only for
    /// modules that opt in with a golangci config at the go.mod root.
    fn enable_golangci(root: &Path) {
        std::fs::write(
            root.join(".golangci.yml"),
            "linters:\n  default: standard\n",
        )
        .unwrap();
    }

    /// A provider whose `govet` option is `addr` (the option is an addr: a build
    /// target for a from-source tool, or a download target).
    fn provider_with_govet(root: PathBuf, addr: &str) -> Provider {
        Provider::with_config(
            root,
            Config {
                govet: addr.to_string(),
                ..Default::default()
            },
        )
        .expect("build provider")
    }

    /// A `govet` build addr resolves verbatim, with the *host's* factors added:
    /// the tool always runs natively, whatever platform the analyzed code targets.
    #[test]
    fn test_govet_tool_addr_source_build_gets_host_factors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = provider_with_govet(tmp.path().to_path_buf(), GOVET_SOURCE_ADDR);
        let addr = p.inner.govet_tool_addr().expect("resolve govet addr");
        assert_eq!(addr.package.as_str(), GOVET_TOOL_PKG);
        assert_eq!(addr.name, "build");
        assert_eq!(addr.args.get("goos"), Some(&current_goos()));
        assert_eq!(addr.args.get("goarch"), Some(&current_goarch()));
    }

    #[test]
    fn test_govet_tool_addr_defaults_to_the_release_download_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = provider_with_govet(
            tmp.path().to_path_buf(),
            "//@heph/go/govet/v0.1.234:heph-govet",
        );
        let addr = p.inner.govet_tool_addr().expect("resolve govet addr");
        assert_eq!(addr.package.as_str(), "@heph/go/govet/v0.1.234");
        assert_eq!(addr.name, govet::GOVET_NAME);
        // The download target renders its URL from these.
        assert_eq!(addr.args.get("goos"), Some(&current_goos()));
    }

    /// Explicit args win over the host default — e.g. pinning a linux tool binary.
    #[test]
    fn test_govet_tool_addr_keeps_explicit_args() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = provider_with_govet(
            tmp.path().to_path_buf(),
            "//@heph/go/govet/v0.1.234:heph-govet@goos=linux,goarch=amd64",
        );
        let addr = p.inner.govet_tool_addr().expect("resolve govet addr");
        assert_eq!(addr.args.get("goos"), Some(&"linux".to_string()));
        assert_eq!(addr.args.get("goarch"), Some(&"amd64".to_string()));
    }

    /// A dev build's default addr points at a release tag no CI run ever published.
    /// *Resolving* it fails with the fix in the message — but only then: naming it
    /// (what every lint/format spec does) must stay fine, or a `query` / `//...`
    /// spec walk would die on any dev build. See the sibling test below.
    #[tokio::test]
    async fn test_get_govet_dev_default_target_is_a_config_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = Provider::new(tmp.path().to_path_buf()).expect("provider");
        let dev = govet::govet_addr(hcore::version::VERSION);
        let err = match provider_get(&p, dev).await {
            Err(GetError::Other(e)) => e,
            Err(other) => panic!("expected a config error, got {other:?}"),
            Ok(_) => panic!("a dev build has no released heph-govet to resolve"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("dev build"), "got: {msg}");
        assert!(msg.contains("//tools/heph-govet:build"), "got: {msg}");
    }

    /// The lint/format specs of a dev build still resolve: they only *name* the
    /// govet addr. A bulk spec walk (`heph query`, `//...`) asks every target for
    /// its spec, and a dev build has no business owning a heph-govet to do that.
    #[tokio::test]
    async fn test_lint_and_format_specs_resolve_on_a_dev_build() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        // Default `govet` — i.e. the dev build's (nonexistent) release target.
        let p = Provider::new(sandbox.path().to_path_buf()).expect("provider");
        // `_lint-analyze` is the per-variant unit; the gate/fixer and the
        // formatters are bare.
        provider_get(&p, make_addr("cmd", "_lint-analyze"))
            .await
            .unwrap_or_else(|e| panic!("_lint-analyze spec must resolve on a dev build: {e:?}"));
        for name in ["lint-check", "lint", "format-check", "format"] {
            provider_get(&p, make_bare_addr("cmd", name))
                .await
                .unwrap_or_else(|e| panic!("{name} spec must resolve on a dev build: {e:?}"));
        }
    }

    #[test]
    fn test_govet_tool_addr_rejects_a_non_addr() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = provider_with_govet(tmp.path().to_path_buf(), "source");
        let err = p.inner.govet_tool_addr().expect_err("not an addr");
        assert!(
            format!("{err:#}").contains("must be a target addr"),
            "got: {err:#}"
        );
    }

    /// The synthesized download target: `//@heph/go/govet/<tag>:heph-govet` is an
    /// `http_fetch` over the release asset. It resolves without a `go list` (it is
    /// answered before package decoding, like the toolchain), and the URL templates
    /// over the addr args so the driver fetches the host's asset.
    #[tokio::test]
    async fn test_get_govet_download_target_is_an_http_fetch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = Provider::new(tmp.path().to_path_buf()).expect("provider");
        let resp = provider_get(&p, govet::govet_addr("v0.1.234"))
            .await
            .expect("govet target resolves");
        assert_eq!(resp.target_spec.driver, "http_fetch");
        assert!(matches!(
            resp.target_spec.config.get("url"),
            Some(Value::String(u)) if u.ends_with("/v0.1.234/heph-govet_{goos}_{goarch}")
        ));
        assert_eq!(
            resp.target_spec.config.get("executable"),
            Some(&Value::Bool(true))
        );
    }

    /// An explicit `checksums` entry pins the binary of a tag this build knows
    /// nothing about (the plugin's own release checksums are baked in at compile
    /// time, so they need no config).
    #[tokio::test]
    async fn test_get_govet_download_target_uses_configured_checksum() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (goos, goarch) = (current_goos(), current_goarch());
        let p = Provider::with_config(
            tmp.path().to_path_buf(),
            Config {
                sdk_checksums: HashMap::from([(
                    govet::checksum_key("v0.1.234", &goos, &goarch),
                    "cafebabe".to_string(),
                )]),
                ..Default::default()
            },
        )
        .expect("provider");
        let resp = provider_get(&p, govet::govet_addr("v0.1.234"))
            .await
            .expect("govet target resolves");
        assert!(matches!(
            resp.target_spec.config.get("sha256"),
            Some(Value::String(s)) if s == "cafebabe"
        ));
    }

    #[test]
    fn collect_go_packages_respects_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("go.mod"), "module example.com/x\n").unwrap();
        // root/heph-home (core skip dir, non-dotted so the dot-rule can't mask
        // it), root/internal (glob skip), root/src (kept).
        let home = root.join("heph-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(root.join("internal")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // Built-in (byte-compared) prunes: dot/underscore prefixes and the
        // go-convention `vendor` / `testdata` dirs.
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::create_dir_all(root.join("_ignored")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::create_dir_all(root.join("testdata")).unwrap();

        let skip = Ignore::new(&[home.clone()], &["internal".to_string()]).unwrap();
        let walker = CachedWalker::disabled();
        let mut out = Vec::new();
        collect_go_packages(&walker, root, root, false, &skip, &mut out);
        let pkgs: Vec<String> = out
            .into_iter()
            .map(|r| r.unwrap().pkg.to_string())
            .collect();

        assert!(pkgs.contains(&"".to_string()));
        assert!(pkgs.contains(&"src".to_string()));
        assert!(
            !pkgs.contains(&"heph-home".to_string()),
            "core dir not pruned"
        );
        assert!(!pkgs.contains(&"internal".to_string()), "glob not pruned");
        for pruned in [".hidden", "_ignored", "vendor", "testdata"] {
            assert!(
                !pkgs.contains(&pruned.to_string()),
                "built-in prune rule dropped: {pruned}"
            );
        }
    }

    #[test]
    fn collect_go_packages_through_enabled_walker_matches_uncached() {
        // The package walk reads each dir through the shared CachedWalker. With a
        // real (enabled) walker backing it the discovered package set must be
        // identical to a raw, uncached walk — the walker is transparent, it only
        // caches the `readdir` by directory mtime.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("go.mod"), "module example.com/x\n").unwrap();
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();

        let skip = Ignore::default();
        let to_pkgs = |v: Vec<anyhow::Result<ListPackageResponse>>| {
            let mut s: Vec<String> = v.into_iter().map(|r| r.unwrap().pkg.to_string()).collect();
            s.sort();
            s
        };

        let mut uncached = Vec::new();
        collect_go_packages(
            &CachedWalker::disabled(),
            root,
            root,
            false,
            &skip,
            &mut uncached,
        );

        let dbdir = tempfile::tempdir().unwrap();
        let walker = CachedWalker::open(&dbdir.path().join("fswalk.db"));
        let mut cached = Vec::new();
        collect_go_packages(&walker, root, root, false, &skip, &mut cached);
        // Second walk: now served from the walker's cache; still identical.
        let mut cached2 = Vec::new();
        collect_go_packages(&walker, root, root, false, &skip, &mut cached2);

        let expected = to_pkgs(uncached);
        assert!(expected.contains(&"a".to_string()));
        assert!(expected.contains(&"a/b".to_string()));
        assert!(!expected.iter().any(|p| p == "vendor"));
        assert_eq!(expected, to_pkgs(cached));
        assert_eq!(expected, to_pkgs(cached2));
    }

    #[test]
    fn compose_closures_dedups_diamond_and_keeps_deps_first() {
        // T1.3 froze the closure-composition contract while switching import paths
        // to `Arc<str>`: children's transitive libs come first, a diamond dep
        // appears once (first-seen wins), and the composing node's own lib is
        // appended last. `import_closure` and `collect_libs_inner` both route
        // through `compose_closures`, so this freezes both.
        let mk = |ip: &str| {
            (
                Arc::<str>::from(ip),
                Addr::new(PkgBuf::from("p"), format!("lib_{ip}"), Default::default()),
            )
        };
        // Diamond: top → {left, right}; left → shared; right → shared.
        let left = Arc::new(ImportClosure {
            libs: vec![mk("shared"), mk("left")],
        });
        let right = Arc::new(ImportClosure {
            libs: vec![mk("shared"), mk("right")],
        });

        let composed = compose_closures(&[left, right], Some(mk("top")));
        let ips: Vec<&str> = composed.iter().map(|(ip, _)| ip.as_ref()).collect();
        assert_eq!(
            ips,
            ["shared", "left", "right", "top"],
            "deps-first, diamond 'shared' deduped to its first occurrence, self last"
        );

        // Without a self lib (the `collect_libs_inner` shape), only the merged,
        // deduped child set remains.
        let a = Arc::new(ImportClosure {
            libs: vec![mk("x"), mk("y")],
        });
        let b = Arc::new(ImportClosure {
            libs: vec![mk("y"), mk("z")],
        });
        let merged = compose_closures(&[a, b], None);
        let ips: Vec<&str> = merged.iter().map(|(ip, _)| ip.as_ref()).collect();
        assert_eq!(ips, ["x", "y", "z"], "diamond 'y' deduped, order preserved");
    }

    fn make_get_req(addr: Addr, workspace_root: &std::path::Path) -> GetRequest {
        GetRequest {
            request_id: "test".to_string(),
            addr,
            states: vec![host_variant_state()],
            executor: test_executor(workspace_root),
        }
    }

    async fn provider_get(p: &Provider, addr: Addr) -> Result<GetResponse, GetError> {
        let ctoken = StdCancellationToken::new();
        let workspace = p.inner.workspace_root.clone();
        p.get(make_get_req(addr, &workspace), &ctoken).await
    }

    #[test]
    fn test_load_go_mod_merges_go_sum_for_indirect_modules() {
        require_go!();
        // An untidied go.mod that directly requires oauth2 but not x/net, with a
        // go.sum that carries x/net's selected version. load_go_mod must expose
        // x/net so that oauth2/internal's import of x/net/context/ctxhttp can
        // resolve to a thirdparty addr instead of being silently dropped.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("go.mod"),
            "module infiot.com/infiot/tools/gogithub\n\ngo 1.12\n\nrequire golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("go.sum"),
            "golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e h1:bRhVy7zSSasaqNksaRZiA5EEI+Ei4I1nO5Jh72wfHlg=\n\
golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e/go.mod h1:mL1N/T3taQHkDXs73rZJwtUhF3w3ftmwwsq0BUmARs4=\n\
golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d h1:pE8b58s1HRDMi8RDc79m0HISf9D4TzseP40cEA6IGfs=\n",
        )
        .unwrap();

        let p = Provider::new(dir.path().to_path_buf()).expect("provider");
        let data = p.inner.load_go_mod(dir.path()).expect("load_go_mod");

        let net = find_module_for_import("golang.org/x/net/context/ctxhttp", &data.requires)
            .expect("x/net must resolve via go.sum even though go.mod omits it");
        assert_eq!(net.0, "golang.org/x/net");
        assert_eq!(net.1, "v0.0.0-20190108225652-1e06a53dbb7e");

        // go.mod-listed module still resolves, and only once (no go.sum dup).
        let oauth2 = find_module_for_import("golang.org/x/oauth2", &data.requires).unwrap();
        assert_eq!(oauth2.1, "v0.0.0-20200107190931-bf48bf16ab8d");
        let oauth2_count = data
            .requires
            .iter()
            .filter(|(m, _)| m == "golang.org/x/oauth2")
            .count();
        assert_eq!(
            oauth2_count, 1,
            "go.mod module must not be duplicated by go.sum"
        );
    }

    // ---- simple_lib ----

    #[tokio::test]
    async fn test_simple_lib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "go_compile");
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_out_has_a_group() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let out = resp.target_spec.config.get("out").unwrap();
        assert!(matches!(out, Value::Map(m) if m.contains_key("a")));
    }

    #[tokio::test]
    async fn test_simple_lib_no_build_target() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("", "build")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_simple_lib_golist_target() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        // _golist should return a spec without calling executor
        let ctoken = StdCancellationToken::new();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![host_variant_state()],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
                states: vec![host_variant_state()],
            }),
        };
        let resp = p.get(req, &ctoken).await.unwrap();
        assert_eq!(resp.target_spec.driver, "go_golist");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("pkg"));
    }

    // Regression: pkg_cache used to wrap executor.result inside the once
    // closure, so cache-hit waiters bypassed the executor entirely. A target-
    // dep cycle could then hide as a memoizer deadlock instead of surfacing
    // as a synchronous CycleError. The fix hoists executor.result out of the
    // closure; every caller must route through it so result_addr's
    // dep_dag.add_dep runs for waiters too.
    #[tokio::test]
    async fn read_golist_package_calls_executor_for_every_caller() {
        require_go!();

        struct CountingExecutor {
            inner: Arc<dyn ProviderExecutor>,
            result_calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ProviderExecutor for CountingExecutor {
            fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                self.result_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.result(addr)
            }
            fn query<'a>(
                &'a self,
                m: &'a hmodel::htmatcher::Matcher,
                extra_skip: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                self.inner.query(m, extra_skip)
            }
        }

        let sandbox = copy_fixture("simple_lib");
        let inner: Arc<dyn ProviderExecutor> = Arc::new(GoListTestExecutor {
            workspace_root: sandbox.path().to_path_buf(),
            source_map: HashMap::new(),
            states: vec![host_variant_state()],
        });
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor: Arc<dyn ProviderExecutor> = Arc::new(CountingExecutor {
            inner,
            result_calls: Arc::clone(&counter),
        });

        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let golist = make_addr("", "_golist");

        p.inner
            .read_golist_package(Arc::clone(&executor), &golist)
            .await
            .unwrap();
        p.inner
            .read_golist_package(Arc::clone(&executor), &golist)
            .await
            .unwrap();

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "every read_golist_package must call executor.result so all \
             callers register the parent → golist edge in DepDag"
        );
    }

    // Regression: a target name the go provider doesn't own (e.g. a buildfile
    // codegen target sharing a Go package dir) must resolve to NotFound WITHOUT
    // resolving `_golist`. Otherwise the `label(go_src)` query that `_golist`
    // pulls in re-enters get_spec for this same addr, trips a false CycleError,
    // and `Engine::query` silently drops the target — so `q codegen .` misses
    // targets that `q all .` (addr-only match, no get_spec) finds. No `go`
    // binary needed: a correct provider bails before any `go list`.
    #[tokio::test]
    async fn foreign_target_name_not_found_without_golist_resolve() {
        // `Provider::new` resolves GOROOT via `go env`, so this needs `go` even
        // though the guard short-circuits before any go list / query.
        require_go!();

        // `Provider::new` enables the foreign-name guard by default, so a name
        // this provider doesn't own resolves to NotFound without touching the
        // executor (no `go list`, no `label(go_src)` query).
        struct BailExecutor {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ProviderExecutor for BailExecutor {
            fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let a = addr.format();
                Box::pin(async move { anyhow::bail!("BailExecutor: result called for {a}") })
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _extra_skip: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { anyhow::bail!("BailExecutor: query called") })
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("go.mod"), "module example.com/x\n").unwrap();
        let p = Provider::new(dir.path().to_path_buf()).unwrap();

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor: Arc<dyn ProviderExecutor> = Arc::new(BailExecutor {
            calls: Arc::clone(&calls),
        });
        let ctoken = StdCancellationToken::new();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "codegen_gen"),
            states: vec![host_variant_state()],
            executor,
        };

        let res = p.get(req, &ctoken).await;
        assert!(
            matches!(res, Err(GetError::NotFound)),
            "foreign name must be NotFound, got driver: {:?}",
            res.map(|r| r.target_spec.driver)
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "go provider must not resolve _golist (or its go_src query) for a \
             target name it doesn't own — that re-entry is what trips the false cycle"
        );
    }

    // ---- with_dep ----

    #[tokio::test]
    async fn test_with_dep_lib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("lib", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_compile");
    }

    #[tokio::test]
    async fn test_with_dep_cmd_build_lib_has_dep_on_lib() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("cmd", "build_lib"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let has_lib_dep = deps.keys().any(|k| k.contains("lib"));
        assert!(
            has_lib_dep,
            "cmd build_lib should depend on lib: got {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_with_dep_lint_driver_and_facts_wiring() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        // From-source govet, as heph's own repo configures it.
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        // The analyze target (`_lint-analyze`) runs heph-govet and produces facts+report.
        let resp = provider_get(&p, make_addr("cmd", "_lint-analyze"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_lint");

        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        // The unitchecker binary is staged for every analyze target.
        assert!(
            deps.keys().any(|k| k == "govet_tool"),
            "_lint-analyze must stage the heph-govet tool: got {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        // `govet` points at a build target, so the staged tool is the one built
        // from heph's own workspace sources.
        let govet_dep = match deps.get("govet_tool").expect("govet_tool group") {
            Value::List(items) => match items.first().expect("one govet dep") {
                Value::String(s) => s.clone(),
                other => panic!("govet dep must be a string, got {other:?}"),
            },
            other => panic!("govet_tool must be a list, got {other:?}"),
        };
        assert!(
            govet_dep.starts_with("//tools/heph-govet:build"),
            "source govet must build the in-workspace tool: got {govet_dep}"
        );

        // cmd imports the first-party `lib`, so its facts feed cmd's analysis:
        // a `facts_*` group must reference lib's `_lint-analyze` target (not its archive).
        let has_lib_facts = deps.iter().any(|(k, v)| {
            k.starts_with("facts_")
                && matches!(v, Value::List(items) if items.iter().any(|it|
                    matches!(it, Value::String(s) if s.contains(":_lint-analyze"))))
        });
        assert!(
            has_lib_facts,
            "cmd _lint-analyze must consume lib's facts via a facts_* group: got {:?}",
            deps.keys().collect::<Vec<_>>()
        );

        // Both the facts and report artifacts are declared outputs.
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected out map"),
        };
        assert!(out.contains_key("facts") && out.contains_key("report"));
    }

    /// With `govet` left at its default (the release download target — what every
    /// consuming workspace gets), the analyze and format targets stage the
    /// *downloaded* heph-govet, not a from-source build: a workspace consuming the
    /// plugin has no `tools/heph-govet` package to build.
    #[tokio::test]
    async fn test_lint_and_format_stage_downloaded_govet_by_default() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = provider_with_govet(
            sandbox.path().to_path_buf(),
            "//@heph/go/govet/v0.1.234:heph-govet",
        );

        for name in ["_lint-analyze", "format-check", "format"] {
            // Formatting is variant-free; lint is not.
            let addr = if name.starts_with("format") {
                make_bare_addr("cmd", name)
            } else {
                make_addr("cmd", name)
            };
            let resp = provider_get(&p, addr).await.unwrap();
            let deps = match resp.target_spec.config.get("deps").unwrap() {
                Value::Map(m) => m,
                other => panic!("{name}: expected deps map, got {other:?}"),
            };
            let govet_dep = match deps.get("govet_tool").expect("govet_tool group") {
                Value::List(items) => match items.first().expect("one govet dep") {
                    Value::String(s) => s.clone(),
                    other => panic!("{name}: govet dep must be a string, got {other:?}"),
                },
                other => panic!("{name}: govet_tool must be a list, got {other:?}"),
            };
            assert!(
                govet_dep.starts_with("//@heph/go/govet/v0.1.234:heph-govet"),
                "{name} must stage the downloaded heph-govet: got {govet_dep}"
            );
        }
    }

    // The user-facing `lint` target is the gate: it depends on `_lint-analyze`'s report
    // output and fails on findings, leaving fact production (and caching) to the
    // always-exit-0 analyze target.
    #[tokio::test]
    async fn test_lint_gate_depends_on_analyze_report() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_bare_addr("cmd", "lint-check"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_lint_gate");

        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let report = match deps.get("report").unwrap() {
            Value::List(l) => l,
            _ => panic!("report not a list"),
        };
        match &report[0] {
            Value::String(s) => assert!(
                s.contains(":_lint-analyze") && s.ends_with("|report"),
                "gate must consume the analyze target's report output: {s}"
            ),
            _ => panic!("not a string"),
        }
    }

    // The user-facing `lint` (fixer) target consumes `_lint-analyze`'s report (for the
    // suggested fixes) plus the package sources, and rewrites the sources in
    // place. It resolves through the go provider like the gate.
    #[tokio::test]
    async fn test_lint_fix_consumes_report_and_sources() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_bare_addr("cmd", "lint"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_lint_fix");

        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        // Report dep points at the analyze target's report output.
        match deps.get("report").unwrap() {
            Value::List(l) => match &l[0] {
                Value::String(s) => assert!(
                    s.contains(":_lint-analyze") && s.ends_with("|report"),
                    "fix must consume the analyze target's report output: {s}"
                ),
                _ => panic!("report not a string"),
            },
            _ => panic!("report not a list"),
        }
        // Default group carries the package's own sources (rewritten in place).
        match deps.get("").unwrap() {
            Value::List(l) => assert!(!l.is_empty(), "fix must stage package sources"),
            _ => panic!("sources not a list"),
        }
        // Declares its `.go` outputs (parse marks them codegen=in_place).
        match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => assert!(m.contains_key("src"), "fix declares src outputs"),
            _ => panic!("out not a map"),
        }
    }

    // Formatting targets resolve through the go provider: `format` is the check
    // gate (no outputs), `format` rewrites sources in place (declares them).
    #[tokio::test]
    async fn test_format_targets_resolve() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        let check = provider_get(&p, make_bare_addr("cmd", "format-check"))
            .await
            .unwrap();
        assert_eq!(check.target_spec.driver, "go_format_check");
        assert!(
            !check.target_spec.config.contains_key("out"),
            "check gate declares no outputs"
        );

        let fix = provider_get(&p, make_bare_addr("cmd", "format"))
            .await
            .unwrap();
        assert_eq!(fix.target_spec.driver, "go_format");
        // Stages the heph-govet tool + the package's own sources.
        let deps = match fix.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("deps not a map"),
        };
        assert!(deps.iter().any(|(k, _)| k == "govet_tool"), "tool staged");
        match deps.iter().find(|(k, _)| k.is_empty()).map(|(_, v)| v) {
            Some(Value::List(l)) => assert!(!l.is_empty(), "sources staged"),
            _ => panic!("default source group missing"),
        }
        assert!(
            fix.target_spec.config.contains_key("out"),
            "fix declares in_place outputs"
        );
    }

    /// Formatting is syntactic: it must cover every `.go` file in the package,
    /// including the ones the current variant's build constraints exclude.
    /// Sourcing the list from `_golist` (i.e. `go list`'s `GoFiles`) left
    /// `foo_windows.go` unformatted forever on a workspace with no windows
    /// variant — and `_test.go` files unformatted everywhere.
    #[tokio::test]
    async fn test_format_covers_constraint_excluded_and_test_files() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        // Neither of these is in `go list`'s GoFiles for the host variant: the
        // first is excluded by its filename build constraint (no test host is
        // both windows and plan9), the second is a test file.
        let pkg_dir = sandbox.path().join("cmd");
        std::fs::write(
            pkg_dir.join("only_windows.go"),
            "//go:build windows && plan9\n\npackage main\n",
        )
        .unwrap();
        std::fs::write(pkg_dir.join("main_test.go"), "package main\n").unwrap();
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        let fix = provider_get(&p, make_bare_addr("cmd", "format"))
            .await
            .unwrap();
        let out = match fix.target_spec.config.get("out").expect("out") {
            Value::Map(m) => match m.iter().find(|(k, _)| *k == "src").map(|(_, v)| v) {
                Some(Value::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => panic!("out entry not a string: {other:?}"),
                    })
                    .collect::<Vec<_>>(),
                other => panic!("src group missing: {other:?}"),
            },
            other => panic!("out not a map: {other:?}"),
        };
        for expected in ["only_windows.go", "main_test.go", "main.go"] {
            assert!(
                out.iter().any(|f| f == expected),
                "{expected} must be formatted: {out:?}"
            );
        }
    }

    /// A state declaring `host` plus a second, cross-compiled variant — the
    /// two-variant ancestry the aggregation tests need.
    fn two_variant_state() -> State {
        let cross = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String("windows".into())),
            ("goarch".to_string(), Value::String("amd64".into())),
        ]));
        let host = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String(current_goos())),
            ("goarch".to_string(), Value::String(current_goarch())),
        ]));
        State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: HashMap::from([(
                "variants".to_string(),
                Value::Map(HashMap::from([
                    ("host".to_string(), host),
                    ("win".to_string(), cross),
                ])),
            )]),
        }
    }

    /// `provider_get` with a custom variant declaration — threaded into both the
    /// request's ancestry states and the executor, so the mocked `_golist` runs
    /// under each variant's own factors.
    async fn provider_get_with_states(
        p: &Provider,
        addr: Addr,
        states: Vec<State>,
    ) -> Result<GetResponse, GetError> {
        let ctoken = StdCancellationToken::new();
        let workspace = p.inner.workspace_root.clone();
        p.get(
            GetRequest {
                request_id: "test".to_string(),
                addr,
                states: states.clone(),
                executor: Arc::new(GoListTestExecutor {
                    workspace_root: workspace,
                    source_map: HashMap::new(),
                    states,
                }),
            },
            &ctoken,
        )
        .await
    }

    /// The string entries of a spec's dep group.
    fn dep_group(spec: &hplugin::provider::TargetSpec, group: &str) -> Vec<String> {
        let deps = match spec.config.get("deps").expect("deps") {
            Value::Map(m) => m,
            other => panic!("deps not a map: {other:?}"),
        };
        match deps.get(group) {
            Some(Value::List(l)) => l
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => panic!("dep entry not a string: {other:?}"),
                })
                .collect(),
            other => panic!("dep group `{group}` missing: {other:?}"),
        }
    }

    /// The gate aggregates: one `_lint-analyze` report dep per declared variant,
    /// off a single bare addr. Selecting one variant instead would report on
    /// whichever subset of the package's files that variant's build constraints
    /// happen to admit.
    #[tokio::test]
    async fn test_lint_check_aggregates_every_variants_report() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        let resp = provider_get_with_states(
            &p,
            make_bare_addr("cmd", "lint-check"),
            vec![two_variant_state()],
        )
        .await
        .unwrap();
        assert_eq!(resp.target_spec.driver, "go_lint_gate");

        let reports = dep_group(&resp.target_spec, "report");
        assert_eq!(
            reports.len(),
            2,
            "one analyze report per declared variant: {reports:?}"
        );
        for v in ["v=host", "v=win"] {
            assert!(
                reports.iter().any(|r| r.contains(":_lint-analyze")
                    && r.contains(v)
                    && r.ends_with("|report")),
                "gate must consume {v}'s analyze report: {reports:?}"
            );
        }
    }

    /// The fixer unions its sources across variants: a file only one variant's
    /// build constraints admit is still analyzed under that variant, so its fix
    /// must have somewhere to land. Under the old per-variant fixer it was both
    /// unfixable from the other variant AND claimed as a `codegen = in_place`
    /// output by two targets at once.
    #[tokio::test]
    async fn test_lint_fix_unions_sources_across_variants() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        // One file per variant, each invisible to the other's `go list`. Note the
        // filenames: `_windows.go` is itself a build constraint, so the
        // non-windows file must not be named `not_windows.go`.
        let pkg_dir = sandbox.path().join("cmd");
        std::fs::write(
            pkg_dir.join("only_windows.go"),
            "//go:build windows\n\npackage main\n",
        )
        .unwrap();
        std::fs::write(
            pkg_dir.join("elsewhere.go"),
            "//go:build !windows\n\npackage main\n",
        )
        .unwrap();
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        let resp =
            provider_get_with_states(&p, make_bare_addr("cmd", "lint"), vec![two_variant_state()])
                .await
                .unwrap();
        assert_eq!(resp.target_spec.driver, "go_lint_fix");

        let out = match resp.target_spec.config.get("out").expect("out") {
            Value::Map(m) => match m.iter().find(|(k, _)| *k == "src").map(|(_, v)| v) {
                Some(Value::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => panic!("out entry not a string: {other:?}"),
                    })
                    .collect::<Vec<_>>(),
                other => panic!("src group missing: {other:?}"),
            },
            other => panic!("out not a map: {other:?}"),
        };
        for expected in ["only_windows.go", "elsewhere.go", "main.go"] {
            assert!(
                out.iter().any(|f| f == expected),
                "{expected} must be a fixable output: {out:?}"
            );
        }
        // Each path claimed exactly once — the whole point of one fixer instead
        // of one per variant.
        let mut sorted = out.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            out.len(),
            "duplicate in_place claims: {out:?}"
        );
        // Sources and outputs stay index-aligned and deduped the same way.
        assert_eq!(dep_group(&resp.target_spec, "").len(), out.len());
    }

    /// A stale `lint@v=NAME` (from when the gate/fixer were variant-selected)
    /// must fail loudly rather than silently linting a now-wider set of files.
    #[tokio::test]
    async fn test_lint_rejects_a_variant_arg() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        for name in ["lint", "lint-check"] {
            let msg = match provider_get(&p, make_addr("cmd", name)).await {
                Err(GetError::Other(e)) => format!("{e:#}"),
                Err(other) => panic!("expected a typed error for {name}, got {other:?}"),
                Ok(_) => panic!("a variant arg on {name} must be rejected"),
            };
            assert!(
                msg.contains("variant-independent") && msg.contains("_lint-analyze"),
                "error must explain the aggregation and name the per-variant unit: {msg}"
            );
        }
    }

    /// None of the user-facing lint/format targets carry a variant, so `list`
    /// emits exactly one bare addr each no matter how many variants a module
    /// declares. `format` is variant-free outright; `lint-check`/`lint` aggregate
    /// the per-variant `_lint-analyze` units, which stay internal (unlisted).
    #[tokio::test]
    async fn list_emits_lint_and_format_once_and_variant_free() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        // Two variants in ancestry: `host` (so runnable targets list) and a
        // second, cross-compiled one.
        let cross = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String("linux".into())),
            ("goarch".to_string(), Value::String("arm64".into())),
        ]));
        let host = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String(current_goos())),
            ("goarch".to_string(), Value::String(current_goarch())),
        ]));
        let states = vec![State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: HashMap::from([(
                "variants".to_string(),
                Value::Map(HashMap::from([
                    ("host".to_string(), host),
                    ("cross".to_string(), cross),
                ])),
            )]),
        }];
        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from("cmd"),
            states,
            executor: test_executor(&p.inner.workspace_root),
        };
        let addrs: Vec<Addr> = p
            .list(req, &StdCancellationToken::new())
            .await
            .unwrap()
            .map(|r| r.unwrap().addr)
            .collect();

        for name in ["format", "format-check", "lint", "lint-check"] {
            let listed: Vec<&Addr> = addrs.iter().filter(|a| a.name == name).collect();
            assert_eq!(
                listed.len(),
                1,
                "{name} must be listed once, not per variant: {listed:?}"
            );
            assert!(
                listed[0].args.is_empty(),
                "{name} must carry no variant args: {:?}",
                listed[0]
            );
        }
        // The variant-scoped analysis unit stays internal: the aggregators pull
        // it in as a dep, so listing it too would run every variant's analysis
        // twice over in a `//...` sweep.
        assert!(
            !addrs.iter().any(|a| a.name == "_lint-analyze"),
            "_lint-analyze must stay unlisted: {addrs:?}"
        );
        // The contrast: `build` genuinely is variant-selected, so it stays
        // multiplied across the two declared variants (plus the bare host-default).
        assert_eq!(
            addrs
                .iter()
                .filter(|a| a.name == "build" && !a.args.is_empty())
                .count(),
            2,
            "build must still be listed per variant: {addrs:?}"
        );
    }

    /// A stale `format@v=NAME` (e.g. carried over from when formatting was
    /// variant-parameterized) must fail loudly, not silently format something
    /// else.
    #[tokio::test]
    async fn test_format_rejects_a_variant_arg() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        enable_golangci(sandbox.path());
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        let msg = match provider_get(&p, make_addr("cmd", "format")).await {
            Err(GetError::Other(e)) => format!("{e:#}"),
            Err(other) => panic!("expected a typed error, got {other:?}"),
            Ok(_) => panic!("a variant arg on format must be rejected"),
        };
        assert!(
            msg.contains("variant-free") && msg.contains("`v`"),
            "error must explain formatting is variant-free: {msg}"
        );
    }

    // Lint/format targets exist ONLY for modules that opt in with a golangci
    // config at the go.mod root. Without one, get resolves them to NotFound and
    // list omits them entirely (while the build/test targets stay).
    #[tokio::test]
    async fn test_lint_targets_absent_without_golangci_config() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        // Deliberately NO `.golangci.yml`.
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);

        for name in [
            "_lint-analyze",
            "lint-check",
            "lint",
            "format-check",
            "format",
        ] {
            // Only the internal `_lint-analyze` unit carries a variant; the
            // user-facing gate/fixer and the formatters are bare.
            let addr = if name == "_lint-analyze" {
                make_addr("cmd", name)
            } else {
                make_bare_addr("cmd", name)
            };
            assert!(
                matches!(provider_get(&p, addr).await, Err(GetError::NotFound)),
                "{name} must be NotFound without a golangci config"
            );
        }

        let names = provider_list(&p, "cmd").await;
        for gated in ["lint-check", "lint", "format-check", "format"] {
            assert!(
                !names.iter().any(|n| n == gated),
                "{gated} must not be listed without a golangci config: {names:?}"
            );
        }
        // The non-lint targets are unaffected.
        assert!(
            names.iter().any(|n| n == "build_lib"),
            "build_lib must still be listed: {names:?}"
        );
    }

    // A `.golangci.yaml` (the other YAML extension) opts a module in just as
    // `.golangci.yml` does.
    #[tokio::test]
    async fn test_lint_enabled_by_golangci_yaml_extension() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        std::fs::write(
            sandbox.path().join(".golangci.yaml"),
            "linters:\n  default: standard\n",
        )
        .unwrap();
        let p = provider_with_govet(sandbox.path().to_path_buf(), GOVET_SOURCE_ADDR);
        provider_get(&p, make_bare_addr("cmd", "lint-check"))
            .await
            .expect("lint-check resolves with a .golangci.yaml");
        let names = provider_list(&p, "cmd").await;
        assert!(
            names.iter().any(|n| n == "lint-check"),
            "lint-check listed with .golangci.yaml: {names:?}"
        );
    }

    // Regression: an imported package whose directory has Go files that are all
    // excluded by build constraints (here a never-set `//go:build` tag, modeling
    // a goos/goarch-excluded package) resolves its `build_lib` to NotFound via
    // NoGoFilesError. Resolving the *importer's* deps must drop that import
    // rather than fail the importer's get_spec — otherwise a query touching the
    // importer dies with "read _golist for …: no Go files in package …".
    #[tokio::test]
    async fn test_dep_constrained_importer_skips_unbuildable_dep() {
        require_go!();
        let sandbox = copy_fixture("dep_constrained");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();

        // The excluded dep itself is NotFound (no buildable Go files).
        let lib_res = provider_get(&p, make_addr("lib", "build_lib")).await;
        assert!(
            matches!(lib_res, Err(GetError::NotFound)),
            "constraint-excluded lib must be NotFound"
        );

        // The importer's get_spec must still succeed, with the unbuildable dep dropped.
        let resp = provider_get(&p, make_addr("cmd", "build_lib"))
            .await
            .expect("cmd build_lib get_spec must succeed despite unbuildable dep");
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        // Dep addrs live in the group values; the excluded lib's `//lib:build_lib`
        // addr must not appear in any group.
        let has_lib = deps.values().any(|v| match v {
            Value::List(items) => items
                .iter()
                .any(|it| matches!(it, Value::String(s) if s.contains("//lib:"))),
            _ => false,
        });
        assert!(
            !has_lib,
            "unbuildable lib must not appear in cmd deps: {:?}",
            deps
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_with_dep_cmd_build_is_main() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("cmd", "build")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(
            out.contains_key(""),
            "build out should have empty-string group: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    // ---- stdlib ----

    #[tokio::test]
    async fn test_stdlib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("@heph/go/std/fmt", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
    }

    #[tokio::test]
    async fn test_stdlib_build_returns_not_found() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("@heph/go/std/fmt", "build")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- with_test ----

    #[tokio::test]
    async fn test_with_test_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_with_test_test_deps_on_build_test() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "test")).await.unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            Value::List(v) => v,
            _ => panic!(),
        };
        let dep_str = match &bin_dep[0] {
            Value::String(s) => s,
            _ => panic!(),
        };
        assert!(
            dep_str.contains("build_test"),
            "dep should reference build_test: {}",
            dep_str
        );
    }

    // ---- with_test_cycle ----
    // pkgb has internal _test.go (no cycle: internal tests can't import pkga
    // because pkga imports pkgb — Go rejects). pkgb also has xtest
    // (`package pkgb_test`) that DOES import pkga — Go allows this because
    // pkgb_test is a distinct package.
    //
    // The bug was: combining internal and xtest into one `build_test` bin
    // forced testmain to reference both P=build_test_lib (for internal tests)
    // AND P_test=build_xtest_lib (for xtest). xtest_lib was compiled against
    // P=normal build_lib, so the linker (providing P=build_test_lib) found a
    // fingerprint mismatch on pkga (which imports P=normal).
    //
    // Fix: split into separate `build_test` (internal) and `build_xtest`
    // (xtest) bins. Internal bin doesn't include pkga at all (Go rejects
    // cycle there). Xtest bin uses P=normal everywhere — testmain_xtest
    // imports `_xtest "P_test"` only, never P directly.

    #[tokio::test]
    async fn test_with_test_cycle_build_test_uses_test_lib_for_p() {
        require_go!();
        let sandbox = copy_fixture("with_test_cycle");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkgb", "build_test"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        // Internal bin's importcfg entry for pkgb's import path must point at
        // build_test_lib (not normal build_lib).
        let pkgb_group = deps
            .iter()
            .find(|(k, _)| k.as_str() == "lib_example_com_with_test_cycle_pkgb")
            .map(|(_, v)| v)
            .expect("build_test deps must include pkgb's lib group");
        let pkgb_addr = match pkgb_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!("expected string"),
            },
            _ => panic!("expected list"),
        };
        assert!(
            pkgb_addr.contains("build_test_lib"),
            "pkgb in internal build_test must reference build_test_lib: got {}",
            pkgb_addr
        );
    }

    #[tokio::test]
    async fn test_with_test_cycle_build_xtest_uses_normal_p_and_xtest_lib() {
        require_go!();
        let sandbox = copy_fixture("with_test_cycle");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkgb", "build_xtest"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        // pkgb (P) must be NORMAL build_lib — xtest_lib was compiled against
        // it; mismatch would occur if xtest bin substituted test_lib here.
        let pkgb_group = deps
            .get("lib_example_com_with_test_cycle_pkgb")
            .expect("xtest bin must include pkgb's lib group");
        let pkgb_addr = match pkgb_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            pkgb_addr.contains("build_lib"),
            "pkgb in xtest bin must use build_lib (normal): got {}",
            pkgb_addr
        );
        assert!(
            !pkgb_addr.contains("build_test_lib"),
            "pkgb in xtest bin must NOT use build_test_lib: got {}",
            pkgb_addr
        );

        // P_test must point at build_xtest_lib.
        let pkgb_test_group = deps
            .get("lib_example_com_with_test_cycle_pkgb_test")
            .expect("xtest bin must include pkgb_test (xtest_lib) group");
        let pkgb_test_addr = match pkgb_test_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            pkgb_test_addr.contains("build_xtest_lib"),
            "pkgb_test must reference build_xtest_lib: got {}",
            pkgb_test_addr
        );

        // pkga (the cycle dep) must be normal build_lib — no `for_test_of` flavoring.
        let pkga_group = deps
            .iter()
            .find(|(k, _)| k.contains("pkga"))
            .map(|(_, v)| v)
            .expect("xtest bin must include pkga");
        let pkga_addr = match pkga_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            !pkga_addr.contains("for_test_of"),
            "pkga must NOT carry for_test_of arg (split bin design): got {}",
            pkga_addr
        );
        assert!(
            !pkga_addr.contains("build_test_lib"),
            "pkga must use normal build_lib: got {}",
            pkga_addr
        );
    }

    // ---- with_embed ----

    #[tokio::test]
    async fn test_with_embed_no_separate_embed_target() {
        // The `embed` target is gone — the go_compile driver resolves the
        // embedcfg in-process.
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("server", "embed")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- factors ----

    #[tokio::test]
    async fn test_variant_factors_flow_to_compile_config() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        // A root variant pinning linux/amd64; resolving `build_lib@v=x` must thread
        // those factors into the go_compile config.
        let variant = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String("linux".into())),
            ("goarch".to_string(), Value::String("amd64".into())),
        ]));
        let state = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: HashMap::from([(
                "variants".to_string(),
                Value::Map(HashMap::from([("x".to_string(), variant)])),
            )]),
        };
        let addr = Addr::new(
            PkgBuf::from(""),
            "build_lib".to_string(),
            VariantRef::new("x", "").to_args(),
        );
        let req = GetRequest {
            request_id: "test".to_string(),
            addr,
            states: vec![state],
            executor: test_executor(sandbox.path()),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let cfg = &resp.target_spec.config;
        assert!(matches!(cfg.get("goos"), Some(Value::String(s)) if s == "linux"));
        assert!(matches!(cfg.get("goarch"), Some(Value::String(s)) if s == "amd64"));
    }

    // ---- with_embed ----

    #[tokio::test]
    async fn test_with_embed_build_lib_exists() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_compile");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("a"));
    }

    #[tokio::test]
    async fn test_with_embed_build_lib_resolves_embed_in_driver() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "build_lib"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        // No separate `embed` target; the compile reads golist's package.bin.
        assert!(
            !deps.contains_key("embed"),
            "build_lib must not dep on a separate embed target"
        );
        assert!(
            deps.contains_key("golist"),
            "embedding build_lib must dep on golist for EmbedPatterns: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
        let variant = match resp.target_spec.config.get("embed_variant").unwrap() {
            Value::List(v) => v,
            _ => panic!("embed_variant must be a list"),
        };
        assert_eq!(variant.len(), 1, "embedding package sets one embed_variant");
        assert!(matches!(&variant[0], Value::String(s) if s == "embed"));
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_no_embed() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(!deps.contains_key("embed"));
        assert!(
            !deps.contains_key("golist"),
            "non-embed build_lib must not dep on golist"
        );
        let variant = match resp.target_spec.config.get("embed_variant").unwrap() {
            Value::List(v) => v,
            _ => panic!("embed_variant must be a list"),
        };
        assert!(variant.is_empty(), "non-embed package has no embed_variant");
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_default_deps_are_pluginfs_addrs() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let src_list = match deps.get("").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            !src_list.is_empty(),
            "default dep group must not be empty for a package with go files"
        );
        for entry in src_list {
            let s = match entry {
                Value::String(s) => s,
                _ => panic!("expected string"),
            };
            assert!(
                s.contains("@heph/fs"),
                "each src dep must be a pluginfs addr, got: {}",
                s
            );
            assert!(
                s.ends_with(".go") || s.contains(".go"),
                "src dep must reference a .go file: {}",
                s
            );
        }
    }

    #[tokio::test]
    async fn test_list_simple_lib_no_go_src_target() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "").await;
        assert!(
            !names.iter().any(|n| n == "_go_src"),
            "_go_src must not appear in list output: {:?}",
            names
        );
    }

    // ---- non-Go package ----

    #[tokio::test]
    async fn test_non_go_package_returns_not_found() {
        require_go!();
        let sandbox = tempfile::tempdir().unwrap();
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("somepkg", "build_lib")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- list() tests ----

    async fn provider_list(p: &Provider, package: &str) -> Vec<String> {
        let ctoken = StdCancellationToken::new();
        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from(package),
            states: vec![host_variant_state()],
            executor: test_executor(&p.inner.workspace_root),
        };
        p.list(req, &ctoken)
            .await
            .unwrap()
            .map(|r| r.unwrap().addr.name.clone())
            .collect()
    }

    /// The completeness fix: a library target must list a variant declared at a
    /// **sibling** package (reachable only via the module universe / `states_under`),
    /// not just the package's own ancestry. Entry targets stay ancestry-scoped.
    #[tokio::test]
    async fn list_library_enumerates_sibling_variant_from_universe() {
        let sandbox = copy_fixture("simple_lib"); // a library package at the module root
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();

        // `release` declared ONLY at sibling package `other` — not in the root
        // lib's ancestry. `states_under` (the module universe) surfaces it.
        struct SiblingUniverse;
        impl ProviderExecutor for SiblingUniverse {
            fn result<'a>(
                &'a self,
                _addr: &'a Addr,
            ) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                unimplemented!("list must not resolve results")
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _s: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                unimplemented!("list must not query")
            }
            fn states_under<'a>(
                &'a self,
                prefix: &'a PkgBuf,
            ) -> BoxFuture<'a, anyhow::Result<Vec<State>>> {
                let release = Value::Map(HashMap::from([
                    ("goos".to_string(), Value::String("linux".into())),
                    ("goarch".to_string(), Value::String("amd64".into())),
                ]));
                let states = if prefix.as_str().is_empty() {
                    vec![State {
                        package: PkgBuf::from("other"),
                        provider: "go".to_string(),
                        state: HashMap::from([(
                            "variants".to_string(),
                            Value::Map(HashMap::from([("release".to_string(), release)])),
                        )]),
                    }]
                } else {
                    vec![]
                };
                Box::pin(async move { Ok(states) })
            }
        }

        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from(""),
            states: vec![], // no ancestry variants for this package
            executor: Arc::new(SiblingUniverse),
        };
        let addrs: Vec<Addr> = p
            .list(req, &StdCancellationToken::new())
            .await
            .unwrap()
            .map(|r| r.unwrap().addr)
            .collect();

        // build_lib (library) lists the sibling's variant, pinned via `vp`.
        assert!(
            addrs.iter().any(|a| a.name == "build_lib"
                && a.args.get("v").map(String::as_str) == Some("release")
                && a.args.get("vp").map(String::as_str) == Some("other")),
            "sibling-declared variant must be listed on build_lib: {addrs:?}"
        );
        // build (entry) is ancestry-scoped — nothing declared in ancestry, so it
        // is not listed.
        assert!(
            !addrs.iter().any(|a| a.name == "build"),
            "entry target must not list a variant absent from ancestry: {addrs:?}"
        );
    }

    /// Module-bounding: `states_under` walks by path prefix, so a variant
    /// declared inside a **nested submodule** (its own `go.mod`) is returned too.
    /// The root module's `list` must NOT enumerate it — that variant is a
    /// different module's target. Only the same-module (root) variant is listed.
    #[tokio::test]
    async fn list_library_excludes_nested_submodule_variant() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("go.mod"),
            "module example.com/root\ngo 1.22\n",
        )
        .unwrap();
        std::fs::create_dir(ws.path().join("nested")).unwrap();
        std::fs::write(
            ws.path().join("nested/go.mod"),
            "module example.com/nested\ngo 1.22\n",
        )
        .unwrap();
        let p = Provider::new(ws.path().to_path_buf()).unwrap();

        // `release` declared at the root module ("") AND at the nested submodule
        // ("nested"). `states_under("")` returns both by prefix.
        struct NestedUniverse;
        impl ProviderExecutor for NestedUniverse {
            fn result<'a>(
                &'a self,
                _addr: &'a Addr,
            ) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                unimplemented!("list must not resolve results")
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _s: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                unimplemented!("list must not query")
            }
            fn states_under<'a>(
                &'a self,
                prefix: &'a PkgBuf,
            ) -> BoxFuture<'a, anyhow::Result<Vec<State>>> {
                let variant = || {
                    Value::Map(HashMap::from([
                        ("goos".to_string(), Value::String("linux".into())),
                        ("goarch".to_string(), Value::String("amd64".into())),
                    ]))
                };
                let go_state = |pkg: &str| State {
                    package: PkgBuf::from(pkg),
                    provider: "go".to_string(),
                    state: HashMap::from([(
                        "variants".to_string(),
                        Value::Map(HashMap::from([("release".to_string(), variant())])),
                    )]),
                };
                let states = if prefix.as_str().is_empty() {
                    vec![go_state(""), go_state("nested")]
                } else {
                    vec![]
                };
                Box::pin(async move { Ok(states) })
            }
        }

        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from(""),
            states: vec![],
            executor: Arc::new(NestedUniverse),
        };
        let addrs: Vec<Addr> = p
            .list(req, &StdCancellationToken::new())
            .await
            .unwrap()
            .map(|r| r.unwrap().addr)
            .collect();

        // The root-module variant is listed (vp="").
        assert!(
            addrs
                .iter()
                .any(|a| a.name == "build_lib" && a.args.get("vp").map(String::as_str) == Some("")),
            "root-module variant must be listed: {addrs:?}"
        );
        // The nested submodule's variant must NOT be — it's a different module.
        assert!(
            !addrs
                .iter()
                .any(|a| a.args.get("vp").map(String::as_str) == Some("nested")),
            "nested submodule variant must not cross the module boundary: {addrs:?}"
        );
    }

    #[tokio::test]
    async fn test_list_with_test_includes_build_test() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "build_test"),
            "expected build_test in list: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_list_with_test_includes_test() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "test"),
            "expected test in list: {:?}",
            names
        );
    }

    // `list` now emits the full candidate set unconditionally for any dir
    // under a go.mod; `get` is what filters by inspecting the `_golist` result.
    // For a package with no `_test.go` files, build_test/test must resolve to
    // NotFound via the per-arm `pkg.test_go_files.is_empty()` guard.
    #[tokio::test]
    async fn test_simple_lib_no_test_targets_via_get() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("", "build_test")).await,
            Err(GetError::NotFound)
        ));
        assert!(matches!(
            provider_get(&p, make_addr("", "test")).await,
            Err(GetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_list_with_test_includes_build_lib() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "build_lib"),
            "expected build_lib in list: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_list_test_only_pkg_includes_build_test() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "build_test"),
            "expected build_test in list for test-only package: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_list_test_only_pkg_includes_test() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "test"),
            "expected test in list for test-only package: {:?}",
            names
        );
    }

    // A test-only package (only `package pkg_test` xtest files) has empty
    // GoFiles, so `build_lib` must resolve to NotFound via the per-arm
    // `pkg.go_files.is_empty()` guard.
    #[tokio::test]
    async fn test_test_only_pkg_build_lib_not_found_via_get() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("pkg", "build_lib")).await,
            Err(GetError::NotFound)
        ));
    }

    // test_only fixture has ONLY xtest_go_files (package pkg_test) — no
    // internal _test.go and no go.go. So `build_test`/`test` (internal-only
    // since the split) return NotFound; the xtest variant exists.
    #[tokio::test]
    async fn test_test_only_build_xtest_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_test_only_xtest_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _resp = provider_get(&p, make_addr("pkg", "xtest")).await.unwrap();
    }

    #[tokio::test]
    async fn test_test_only_internal_build_test_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("pkg", "build_test")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // Regression: xtest-only package's `build_xtest` previously hard-coded P =
    // build_lib in importcfg, even though build_lib doesn't exist for a pkg
    // with no GoFiles. `pick_xtest_p_lib_name` now skips the P slot entirely.
    #[tokio::test]
    async fn xtest_only_build_xtest_omits_p_slot() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let p_group =
            crate::plugingo::addr_util::import_path_to_dep_group("example.com/testonly/pkg");
        assert!(
            !deps.contains_key(&p_group),
            "xtest-only bin must not reference build_lib for P: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    // ---- test_only_internal ----
    // Only internal `package pkg` _test.go file. GoFiles is empty;
    // TestGoFiles is non-empty. build_lib/build_xtest/build_xtest_lib must
    // resolve NotFound; build_test/test/build_test_lib must succeed.

    #[tokio::test]
    async fn test_only_internal_build_lib_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("pkg", "build_lib")).await,
            Err(GetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_only_internal_build_test_lib_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_compile");
    }

    #[tokio::test]
    async fn test_only_internal_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_only_internal_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _ = provider_get(&p, make_addr("pkg", "test")).await.unwrap();
    }

    #[tokio::test]
    async fn test_only_internal_xtest_variants_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        for name in [
            "build_xtest",
            "build_xtest_lib",
            "build_xtestmain_lib",
            "xtest",
            "xtestmain",
        ] {
            assert!(
                matches!(
                    provider_get(&p, make_addr("pkg", name)).await,
                    Err(GetError::NotFound)
                ),
                "{name} must be NotFound for test-only-internal package"
            );
        }
    }

    // ---- test_xtest_only ----
    // Both internal _test.go (package pkg) and external x_test.go
    // (package pkg_test) present. xtest imports the internal package, which
    // has no GoFiles → P's xtest slot must use build_test_lib, not build_lib.

    #[tokio::test]
    async fn test_xtest_only_build_lib_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("pkg", "build_lib")).await,
            Err(GetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_xtest_only_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _ = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_xtest_only_build_xtest_exists() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _ = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
    }

    // P (the internal pkg) has only TestGoFiles, so xtest_lib and xtest bin
    // must both reference build_test_lib for P (not build_lib, which doesn't
    // exist).
    #[tokio::test]
    async fn test_xtest_only_build_xtest_p_slot_uses_test_lib() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let p_group =
            crate::plugingo::addr_util::import_path_to_dep_group("example.com/testxtestonly/pkg");
        let entry = deps
            .get(&p_group)
            .expect("xtest bin must include P's lib group");
        let s = match entry {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            s.contains("build_test_lib"),
            "P in xtest bin must reference build_test_lib for test+xtest-only pkg: {s}"
        );
        assert!(
            !s.contains(":build_lib"),
            "P must NOT reference normal build_lib: {s}"
        );
    }

    #[tokio::test]
    async fn test_xtest_only_build_xtest_lib_p_in_importcfg_uses_test_lib() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest_lib"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let p_group =
            crate::plugingo::addr_util::import_path_to_dep_group("example.com/testxtestonly/pkg");
        let entry = deps
            .get(&p_group)
            .expect("xtest_lib must include P in importcfg (xtest source imports P)");
        let s = match entry {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            s.contains("build_test_lib"),
            "P in xtest_lib must reference build_test_lib for test+xtest-only pkg: {s}"
        );
    }

    // ---- mod-asm ----

    #[tokio::test]
    async fn test_mod_asm_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("mod-asm");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "go_compile");
    }

    #[tokio::test]
    async fn test_mod_asm_thirdparty_with_sfiles_generates_asm_steps() {
        require_go!();
        let sandbox = copy_fixture("mod-asm");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();

        // github.com/klauspost/cpuid/v2 has assembly on all architectures
        let addr = make_addr(
            "@heph/go/thirdparty/github.com/klauspost/cpuid/v2@v2.2.5",
            "build_lib",
        );
        let resp = match provider_get(&p, addr).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping: could not get cpuid build_lib: {:?}", e);
                return;
            }
        };

        let cfg = &resp.target_spec.config;
        let s_files = match cfg.get("s_files").unwrap() {
            Value::List(v) => v,
            _ => panic!("s_files must be a list"),
        };
        // Only assert the asm wiring when the package actually has .s files on
        // this platform; the go_compile driver derives the asm/symabis/pack
        // steps from s_files at run time.
        if !s_files.is_empty() {
            let deps = match cfg.get("deps").unwrap() {
                Value::Map(m) => m,
                _ => panic!("deps must be a map"),
            };
            assert!(
                deps.contains_key("asm"),
                "asm package must stage its .s sources in the `asm` dep group: {:?}",
                deps.keys().collect::<Vec<_>>()
            );
        }
    }

    fn state_with_root(pkg: &str, root: bool) -> State {
        let mut m = HashMap::new();
        m.insert("go_codegen_root".to_string(), Value::Bool(root));
        State {
            package: PkgBuf::from(pkg),
            provider: "go".to_string(),
            state: m,
        }
    }

    #[test]
    fn pick_codegen_root_picks_deepest_matching_state() {
        let states = vec![
            state_with_root("src", true),
            state_with_root("src/foo", true),
            state_with_root("other", true),
        ];
        let picked = pick_codegen_root(&states).unwrap();
        assert_eq!(picked.package.as_str(), "src/foo");
    }

    #[test]
    fn pick_codegen_root_ignores_states_with_root_false() {
        let states = vec![state_with_root("src", false)];
        let picked = pick_codegen_root(&states);
        assert!(picked.is_none());
    }

    #[test]
    fn pick_codegen_root_returns_none_when_no_states() {
        let picked = pick_codegen_root(&[]);
        assert!(picked.is_none());
    }

    #[test]
    fn pick_codegen_root_matches_root_state_at_empty_pkg() {
        let states = vec![state_with_root("", true)];
        let picked = pick_codegen_root(&states).unwrap();
        assert_eq!(picked.package.as_str(), "");
    }

    /// `skip = true` → `test = False` (disabled); `skip = false` → `test = True`.
    fn state_with_test_skip(pkg: &str, skip: bool) -> State {
        let mut m = HashMap::new();
        m.insert("test".to_string(), Value::Bool(!skip));
        State {
            package: PkgBuf::from(pkg),
            provider: "go".to_string(),
            state: m,
        }
    }

    #[test]
    fn pick_test_skip_false_when_no_states() {
        assert!(!pick_test_skip(&[], "foo"));
    }

    #[test]
    fn pick_test_skip_true_when_state_sets_skip_for_own_package() {
        let states = vec![state_with_test_skip("", true)];
        assert!(pick_test_skip(&states, ""));
    }

    #[test]
    fn pick_test_skip_non_recursive_does_not_reach_descendants() {
        // `test = False` at `foo` only disables `foo`'s own tests; `foo/bar` runs.
        let states = vec![state_with_test_skip("foo", true)];
        assert!(pick_test_skip(&states, "foo"));
        assert!(!pick_test_skip(&states, "foo/bar"));
    }

    #[test]
    fn pick_test_skip_recursive_reaches_descendants() {
        let states = vec![with_recursive(state_with_test_skip("foo", true))];
        assert!(pick_test_skip(&states, "foo/bar"));
    }

    #[test]
    fn pick_test_skip_deeper_state_overrides_recursive_ancestor() {
        // Recursive root says test=False (skip); deeper pkg says test=True → run.
        let states = vec![
            with_recursive(state_with_test_skip("", true)),
            state_with_test_skip("src/foo", false),
        ];
        assert!(!pick_test_skip(&states, "src/foo"));
    }

    #[test]
    fn pick_test_skip_struct_form_leaves_tests_enabled() {
        // A deeper struct-form `test = {env: ...}` re-enables tests even when a
        // recursive root disabled them — the struct implies tests run.
        let states = vec![
            with_recursive(state_with_test_skip("", true)),
            state_with_test_map(
                "src/foo",
                vec![(
                    "env",
                    Value::Map(HashMap::from([(
                        "FOO".to_string(),
                        Value::String("1".to_string()),
                    )])),
                )],
            ),
        ];
        assert!(!pick_test_skip(&states, "src/foo"));
    }

    #[test]
    fn pick_test_skip_state_without_test_key_returns_false() {
        let mut m = HashMap::new();
        m.insert("other".to_string(), Value::Bool(true));
        let states = vec![State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: m,
        }];
        assert!(!pick_test_skip(&states, ""));
    }

    fn test_env_is_empty(env: &target_test::TestEnv) -> bool {
        env.env.is_empty()
            && env.runtime_env.is_empty()
            && env.pass_env.is_empty()
            && env.runtime_pass_env.is_empty()
            && env.pre_run.is_empty()
    }

    fn state_with_test_map(pkg: &str, entries: Vec<(&str, Value)>) -> State {
        let test_map: HashMap<String, Value> = entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let mut m = HashMap::new();
        m.insert("test".to_string(), Value::Map(test_map));
        State {
            package: PkgBuf::from(pkg),
            provider: "go".to_string(),
            state: m,
        }
    }

    #[test]
    fn pick_test_env_empty_when_no_states() {
        let env = pick_test_env(&[], "foo").unwrap();
        assert!(test_env_is_empty(&env));
    }

    #[test]
    fn pick_test_env_reads_all_four_knobs() {
        let states = vec![state_with_test_map(
            "foo",
            vec![
                (
                    "env",
                    Value::Map(HashMap::from([(
                        "FOO".to_string(),
                        Value::String("1".to_string()),
                    )])),
                ),
                (
                    "runtime_env",
                    Value::Map(HashMap::from([(
                        "BAR".to_string(),
                        Value::String("2".to_string()),
                    )])),
                ),
                (
                    "pass_env",
                    Value::List(vec![Value::String("HOME".to_string())]),
                ),
                (
                    "runtime_pass_env",
                    Value::List(vec![Value::String("PATH".to_string())]),
                ),
            ],
        )];
        let env = pick_test_env(&states, "foo").unwrap();
        assert_eq!(env.env.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(env.runtime_env.get("BAR").map(String::as_str), Some("2"));
        assert_eq!(env.pass_env, vec!["HOME".to_string()]);
        assert_eq!(env.runtime_pass_env, vec!["PATH".to_string()]);
    }

    #[test]
    fn pick_test_env_applies_only_to_exact_package_not_ancestors() {
        // State at ancestor `foo` must NOT leak into `foo/bar`'s test targets.
        let states = vec![state_with_test_map(
            "foo",
            vec![(
                "env",
                Value::Map(HashMap::from([(
                    "FOO".to_string(),
                    Value::String("1".to_string()),
                )])),
            )],
        )];
        let env = pick_test_env(&states, "foo/bar").unwrap();
        assert!(test_env_is_empty(&env));
    }

    #[test]
    fn pick_test_env_errors_on_non_string_env_value() {
        let states = vec![state_with_test_map(
            "foo",
            vec![(
                "env",
                Value::Map(HashMap::from([("FOO".to_string(), Value::Int(3))])),
            )],
        )];
        assert!(pick_test_env(&states, "foo").is_err());
    }

    #[test]
    fn pick_test_env_errors_on_unknown_key() {
        // `test = {"skip": True}` is a mistake — tests are disabled with the
        // bool `test = False`, not a `skip` key. Must fail closed, not silently
        // ignore the typo.
        let states = vec![state_with_test_map(
            "foo",
            vec![("skip", Value::Bool(true))],
        )];
        let err = pick_test_env(&states, "foo").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("skip"), "error must name the bad key: {msg}");
        assert!(
            msg.contains("test = False"),
            "error must point to the correct disable syntax: {msg}"
        );
    }

    #[test]
    fn pick_test_env_reads_pre_run_lines_in_order() {
        let states = vec![state_with_test_map(
            "foo",
            vec![(
                "pre_run",
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ]),
            )],
        )];
        let env = pick_test_env(&states, "foo").unwrap();
        assert_eq!(env.pre_run, vec!["a", "b", "c"]);
    }

    #[test]
    fn pick_test_env_pre_run_applies_only_to_exact_package() {
        // pre_run at ancestor `foo` must NOT leak into `foo/bar`'s test targets.
        let states = vec![state_with_test_map(
            "foo",
            vec![(
                "pre_run",
                Value::List(vec![Value::String("setup".to_string())]),
            )],
        )];
        let env = pick_test_env(&states, "foo/bar").unwrap();
        assert!(env.pre_run.is_empty());
    }

    #[test]
    fn pick_test_env_errors_on_non_string_pre_run_item() {
        let states = vec![state_with_test_map(
            "foo",
            vec![(
                "pre_run",
                Value::List(vec![Value::String("ok".to_string()), Value::Bool(true)]),
            )],
        )];
        assert!(pick_test_env(&states, "foo").is_err());
    }

    /// Add `recursive = True` to an existing State.
    fn with_recursive(mut s: State) -> State {
        s.state.insert("recursive".to_string(), Value::Bool(true));
        s
    }

    /// A `test` struct `env` knob carrying a single `key=val` pair.
    fn env_entry(key: &str, val: &str) -> (&'static str, Value) {
        (
            "env",
            Value::Map(HashMap::from([(
                key.to_string(),
                Value::String(val.to_string()),
            )])),
        )
    }

    fn state_with_link_map(pkg: &str, entries: Vec<(&str, Value)>) -> State {
        let link_map: HashMap<String, Value> = entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let mut m = HashMap::new();
        m.insert("link".to_string(), Value::Map(link_map));
        State {
            package: PkgBuf::from(pkg),
            provider: "go".to_string(),
            state: m,
        }
    }

    #[test]
    fn pick_test_env_recursive_applies_to_descendants() {
        // `recursive = True` at ancestor `foo` reaches `foo/bar`'s test target.
        let states = vec![with_recursive(state_with_test_map(
            "foo",
            vec![env_entry("FOO", "1")],
        ))];
        let env = pick_test_env(&states, "foo/bar").unwrap();
        assert_eq!(env.env.get("FOO").map(String::as_str), Some("1"));
    }

    #[test]
    fn pick_test_env_deeper_package_overrides_recursive_ancestor() {
        // Recursive ancestor sets FOO=1; the exact package overrides FOO=2.
        let states = vec![
            with_recursive(state_with_test_map("foo", vec![env_entry("FOO", "1")])),
            state_with_test_map("foo/bar", vec![env_entry("FOO", "2")]),
        ];
        let env = pick_test_env(&states, "foo/bar").unwrap();
        assert_eq!(env.env.get("FOO").map(String::as_str), Some("2"));
    }

    #[test]
    fn pick_link_empty_when_no_states() {
        let link = pick_link(&[], "foo").unwrap();
        assert!(link.flags.is_empty() && link.deps.is_empty() && link.runtime_deps.is_empty());
    }

    #[test]
    fn pick_link_reads_all_knobs() {
        let states = vec![state_with_link_map(
            "foo",
            vec![
                ("flags", Value::List(vec![Value::String("-s".to_string())])),
                (
                    "deps",
                    Value::List(vec![Value::String("//a:b".to_string())]),
                ),
                (
                    "runtime_deps",
                    Value::List(vec![Value::String("//c:d".to_string())]),
                ),
            ],
        )];
        let link = pick_link(&states, "foo").unwrap();
        assert_eq!(link.flags, vec!["-s".to_string()]);
        // A plain list lands in the default (empty) group.
        assert_eq!(
            link.deps,
            BTreeMap::from([(String::new(), vec!["//a:b".to_string()])])
        );
        assert_eq!(
            link.runtime_deps,
            BTreeMap::from([(String::new(), vec!["//c:d".to_string()])])
        );
    }

    #[test]
    fn pick_link_deps_accept_named_group_map() {
        let states = vec![state_with_link_map(
            "foo",
            vec![
                (
                    "deps",
                    Value::Map(HashMap::from([(
                        "assets".to_string(),
                        Value::List(vec![Value::String("//a:b".to_string())]),
                    )])),
                ),
                (
                    "runtime_deps",
                    Value::Map(HashMap::from([(
                        "data".to_string(),
                        Value::String("//c:d".to_string()),
                    )])),
                ),
            ],
        )];
        let link = pick_link(&states, "foo").unwrap();
        assert_eq!(
            link.deps,
            BTreeMap::from([("assets".to_string(), vec!["//a:b".to_string()])])
        );
        // A bare string value in a map entry is coerced to a single-element list.
        assert_eq!(
            link.runtime_deps,
            BTreeMap::from([("data".to_string(), vec!["//c:d".to_string()])])
        );
    }

    #[test]
    fn pick_link_recursive_merges_deps_per_group() {
        let states = vec![
            with_recursive(state_with_link_map(
                "foo",
                vec![(
                    "deps",
                    Value::Map(HashMap::from([(
                        "assets".to_string(),
                        Value::List(vec![Value::String("//a:one".to_string())]),
                    )])),
                )],
            )),
            state_with_link_map(
                "foo/bar",
                vec![(
                    "deps",
                    Value::Map(HashMap::from([(
                        "assets".to_string(),
                        Value::List(vec![Value::String("//a:two".to_string())]),
                    )])),
                )],
            ),
        ];
        let link = pick_link(&states, "foo/bar").unwrap();
        // Same group name from ancestor + self accumulates shallow->deep.
        assert_eq!(
            link.deps,
            BTreeMap::from([(
                "assets".to_string(),
                vec!["//a:one".to_string(), "//a:two".to_string()]
            )])
        );
    }

    #[test]
    fn pick_link_exact_package_only_by_default() {
        let states = vec![state_with_link_map(
            "foo",
            vec![("flags", Value::List(vec![Value::String("-s".to_string())]))],
        )];
        let link = pick_link(&states, "foo/bar").unwrap();
        assert!(
            link.flags.is_empty(),
            "non-recursive link must not leak to descendants"
        );
    }

    #[test]
    fn pick_link_recursive_accumulates_ancestor_and_self_flags() {
        let states = vec![
            with_recursive(state_with_link_map(
                "foo",
                vec![("flags", Value::List(vec![Value::String("-s".to_string())]))],
            )),
            state_with_link_map(
                "foo/bar",
                vec![("flags", Value::List(vec![Value::String("-w".to_string())]))],
            ),
        ];
        let link = pick_link(&states, "foo/bar").unwrap();
        // shallow->deep order: ancestor flag first, then the package's own.
        assert_eq!(link.flags, vec!["-s".to_string(), "-w".to_string()]);
    }

    #[test]
    fn pick_link_errors_on_unknown_key() {
        let states = vec![state_with_link_map(
            "foo",
            vec![("bogus", Value::Bool(true))],
        )];
        let err = pick_link(&states, "foo").unwrap_err();
        assert!(
            format!("{err:#}").contains("bogus"),
            "error must name the bad key"
        );
    }

    #[tokio::test]
    async fn list_excludes_test_targets_when_test_skip_set() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let ctoken = StdCancellationToken::new();
        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from("pkg"),
            states: vec![
                with_recursive(state_with_test_skip("", true)),
                host_variant_state(),
            ],
            executor: test_executor(sandbox.path()),
        };
        let names: Vec<String> = p
            .list(req, &ctoken)
            .await
            .unwrap()
            .map(|r| r.unwrap().addr.name.clone())
            .collect();
        for name in TEST_TARGET_NAMES {
            assert!(
                !names.iter().any(|n| n == name),
                "test target {name} must not appear in list when test.skip=True: {names:?}"
            );
        }
        // Non-test targets still emitted.
        assert!(names.iter().any(|n| n == "build_lib"));
        assert!(names.iter().any(|n| n == "_golist"));
    }

    /// Runnable `test`/`xtest` targets execute the built binary, so `list` emits
    /// them only for ancestry variants matching the host `goos`/`goarch`. The
    /// cross-compilable `build_test`/`build_xtest` list for *every* variant.
    #[tokio::test]
    async fn list_runnable_test_targets_only_for_host_variant() {
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();

        // Module root declares two variants: `host` (this machine) and `cross`
        // (host goos, a non-host goarch).
        let host = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String(current_goos())),
            ("goarch".to_string(), Value::String(current_goarch())),
        ]));
        let cross = Value::Map(HashMap::from([
            ("goos".to_string(), Value::String(current_goos())),
            ("goarch".to_string(), Value::String("otherarch".into())),
        ]));
        let two = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: HashMap::from([(
                "variants".to_string(),
                Value::Map(HashMap::from([
                    ("host".to_string(), host),
                    ("cross".to_string(), cross),
                ])),
            )]),
        };

        struct TwoVariantUniverse(State);
        impl ProviderExecutor for TwoVariantUniverse {
            fn result<'a>(
                &'a self,
                _addr: &'a Addr,
            ) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                unimplemented!("list must not resolve results")
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _s: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                unimplemented!("list must not query")
            }
            fn states_under<'a>(
                &'a self,
                prefix: &'a PkgBuf,
            ) -> BoxFuture<'a, anyhow::Result<Vec<State>>> {
                let states = if prefix.as_str().is_empty() {
                    vec![self.0.clone()]
                } else {
                    vec![]
                };
                Box::pin(async move { Ok(states) })
            }
        }

        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from("pkg"),
            states: vec![two.clone()],
            executor: Arc::new(TwoVariantUniverse(two)),
        };
        let addrs: Vec<Addr> = p
            .list(req, &StdCancellationToken::new())
            .await
            .unwrap()
            .map(|r| r.unwrap().addr)
            .collect();

        let variants_of = |name: &str| -> Vec<String> {
            let mut v: Vec<String> = addrs
                .iter()
                .filter(|a| a.name == name)
                .filter_map(|a| a.args.get("v").cloned())
                .collect();
            v.sort();
            v
        };

        assert_eq!(
            variants_of("test"),
            vec!["host".to_string()],
            "runnable test must list only the host variant: {addrs:?}"
        );
        assert_eq!(
            variants_of("xtest"),
            vec!["host".to_string()],
            "runnable xtest must list only the host variant: {addrs:?}"
        );
        assert_eq!(
            variants_of("build_test"),
            vec!["cross".to_string(), "host".to_string()],
            "build_test must list every variant: {addrs:?}"
        );
        assert_eq!(
            variants_of("build_xtest"),
            vec!["cross".to_string(), "host".to_string()],
            "build_xtest must list every variant: {addrs:?}"
        );
        // The magic host-default `build` (bare, no `@v`) is listed once alongside
        // the per-variant `build@v=…`.
        assert!(
            addrs
                .iter()
                .any(|a| a.name == "build" && !a.args.contains_key("v")),
            "magic bare build must be listed when a host variant exists: {addrs:?}"
        );
    }

    /// The magic host-default `build`: a bare `//pkg:build` on a `main` package
    /// resolves to a `group` target forwarding to `build@v=<host-matching variant>`.
    #[tokio::test]
    async fn magic_build_returns_group_forwarding_to_host_variant() {
        require_go!();
        let sandbox = copy_fixture("with_dep"); // `cmd` is `package main`
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let addr = Addr::new(PkgBuf::from("cmd"), "build".to_string(), Default::default());
        let resp = provider_get(&p, addr).await.expect("magic build resolves");
        let spec = resp.target_spec;
        assert_eq!(spec.driver, "group");
        let deps = match spec.config.get("deps") {
            Some(Value::List(v)) => v,
            other => panic!("group deps must be a list, got: {other:?}"),
        };
        assert_eq!(
            deps,
            &vec![Value::String("//cmd:build@v=host".into())],
            "magic build must forward to the host variant"
        );
    }

    /// No ancestry variant matches the host os/arch → the magic bare `build` does
    /// not resolve (there is nothing to forward to). Checked before `go list`.
    #[tokio::test]
    async fn magic_build_not_found_without_host_variant() {
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let cross = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: HashMap::from([(
                "variants".to_string(),
                Value::Map(HashMap::from([(
                    "cross".to_string(),
                    Value::Map(HashMap::from([
                        ("goos".to_string(), Value::String(current_goos())),
                        ("goarch".to_string(), Value::String("otherarch".into())),
                    ])),
                )])),
            )]),
        };
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: Addr::new(PkgBuf::from("cmd"), "build".to_string(), Default::default()),
            states: vec![cross],
            executor: test_executor(sandbox.path()),
        };
        let res = p.get(req, &StdCancellationToken::new()).await;
        assert!(
            matches!(res, Err(GetError::NotFound)),
            "magic build must NotFound when no host variant exists"
        );
    }

    /// Regression: the magic bare `build` on a NON-main package (a library or a
    /// directory that only holds go sub-packages) must resolve to a self-addr
    /// `NotFound` — exactly like the real `build@v=…` — so the codegen/query
    /// walks skip it, rather than emitting a `group` whose `build@v=…` dep can't
    /// resolve (which surfaced as a cross-addr `target not found` in
    /// `heph tool gen-gitignore`).
    #[tokio::test]
    async fn magic_build_not_found_for_non_main_package() {
        require_go!();
        let sandbox = copy_fixture("with_dep"); // `lib` is a library (package lib)
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let addr = Addr::new(PkgBuf::from("lib"), "build".to_string(), Default::default());
        let res = provider_get(&p, addr).await;
        assert!(
            matches!(res, Err(GetError::NotFound)),
            "magic build on a non-main package must NotFound"
        );
    }

    /// Unknown addr args are rejected. A legacy `build@goos=…,goarch=…` (a
    /// pre-variant BUILD dep) must NOT be hijacked by the magic host-default nor
    /// silently ignored — it errors naming the offending arg, pointing at `@v=`.
    #[tokio::test]
    async fn unknown_build_addr_arg_is_rejected() {
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let addr = Addr::new(
            PkgBuf::from("cmd"),
            "build".to_string(),
            std::collections::BTreeMap::from([
                ("goos".to_string(), "linux".to_string()),
                ("goarch".to_string(), "amd64".to_string()),
            ]),
        );
        let err = match provider_get(&p, addr).await {
            Err(GetError::Other(e)) => e,
            Err(GetError::NotFound) => panic!("expected an unknown-arg error, got NotFound"),
            Ok(_) => panic!("expected an error, got Ok"),
        };
        let msg = format!("{err:#}");
        // Args iterate sorted, so `goarch` (the first offending key) is named.
        assert!(
            msg.contains("unknown addr arg `goarch`") && msg.contains("@v="),
            "error must name the bad arg and point at @v=: {msg}"
        );
    }

    /// Unknown args are rejected on non-`build` targets too (the check is general,
    /// not build-specific).
    #[tokio::test]
    async fn unknown_addr_arg_rejected_on_build_lib() {
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let addr = Addr::new(
            PkgBuf::from("lib"),
            "build_lib".to_string(),
            std::collections::BTreeMap::from([
                ("v".to_string(), "host".to_string()),
                ("bogus".to_string(), "x".to_string()),
            ]),
        );
        let err = match provider_get(&p, addr).await {
            Err(GetError::Other(e)) => e,
            other => {
                let _ = other;
                panic!("expected an unknown-arg error")
            }
        };
        assert!(
            format!("{err:#}").contains("unknown addr arg `bogus`"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn get_returns_not_found_for_test_targets_when_test_skip_set() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let ctoken = StdCancellationToken::new();
        for name in ["test", "build_test", "xtest", "build_xtest"] {
            let req = GetRequest {
                request_id: "test".to_string(),
                addr: make_addr("pkg", name),
                states: vec![
                    with_recursive(state_with_test_skip("", true)),
                    host_variant_state(),
                ],
                executor: test_executor(sandbox.path()),
            };
            let res = p.get(req, &ctoken).await;
            assert!(
                matches!(res, Err(GetError::NotFound)),
                "get({name}) must return NotFound when test.skip=True"
            );
        }
    }

    #[tokio::test]
    async fn get_build_test_still_works_when_test_skip_false_overrides() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let ctoken = StdCancellationToken::new();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("pkg", "build_test"),
            states: vec![
                with_recursive(state_with_test_skip("", true)),
                state_with_test_skip("pkg", false),
                host_variant_state(),
            ],
            executor: test_executor(sandbox.path()),
        };
        let res = p.get(req, &ctoken).await;
        assert!(res.is_ok(), "deeper test.skip=False must override");
    }

    fn extract_srcfiles(resp: &GetResponse) -> Vec<String> {
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("deps not a map"),
        };
        match deps.get("srcfiles").unwrap() {
            Value::List(l) => l
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    _ => panic!("srcfiles entry not a string"),
                })
                .collect(),
            _ => panic!("srcfiles not a list"),
        }
    }

    #[tokio::test]
    async fn golist_root_package_keeps_an_exact_go_src_query_scope() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![host_variant_state()],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
                states: vec![host_variant_state()],
            }),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let srcfiles = extract_srcfiles(&resp);
        let go_src_query = srcfiles
            .iter()
            .find(|s| s.contains("label(go_src)"))
            .expect("go_src query addr present");
        // This fixture's go package is the *root* package, whose subtree is the whole
        // workspace (synthetic `//@heph/…` namespaces included) — so it keeps the
        // exact-package scope. Every other package scopes to its subtree, so a
        // generator in a sub-package is reachable; see `default_scope`.
        assert!(
            !go_src_query.contains("..."),
            "the root package must keep an exact scope, got: {go_src_query}"
        );
        // `tree_output` is what keeps the widened scope honest: a target in the
        // subtree is only a source if its codegen output lands in *this* package.
        assert!(
            go_src_query.contains("tree_output("),
            "go_src query must carry tree_output, got: {go_src_query}"
        );
    }

    #[tokio::test]
    async fn golist_codegen_root_widens_go_src_query_and_appends_deps() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let mut state_map = HashMap::new();
        state_map.insert("go_codegen_root".to_string(), Value::Bool(true));
        state_map.insert(
            "go_codegen_deps".to_string(),
            Value::List(vec![Value::String("//codegen:gen".to_string())]),
        );
        let state = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: state_map,
        };
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![state, host_variant_state()],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
                states: vec![host_variant_state()],
            }),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let srcfiles = extract_srcfiles(&resp);
        let go_src_query = srcfiles
            .iter()
            .find(|s| s.contains("label(go_src)"))
            .expect("go_src query addr present");
        // codegen_root widens the scope to a `/...` (here root `//...`) prefix.
        assert!(
            go_src_query.contains("..."),
            "codegen_root must widen to a package prefix, got: {go_src_query}"
        );
        assert!(
            srcfiles.iter().any(|s| s == "//codegen:gen"),
            "go_codegen_deps must be appended to srcfiles, got: {srcfiles:?}"
        );
    }

    #[test]
    fn compute_embed_src_addrs_has_label_query_and_appends_deps() {
        let mut state_map = HashMap::new();
        state_map.insert(
            "go_embed_deps".to_string(),
            Value::List(vec![Value::String("//ui:dist".to_string())]),
        );
        let states = vec![State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: state_map,
        }];
        let addrs = compute_embed_src_addrs("pkg", &states).unwrap();
        assert!(
            addrs
                .iter()
                .any(|s| s.contains("label(go_embed_src)") && s.contains("tree_output(")),
            "must query go_embed_src tree outputs: {addrs:?}"
        );
        assert!(
            addrs.iter().any(|s| s == "//ui:dist"),
            "go_embed_deps must be appended: {addrs:?}"
        );
    }

    // Cheapest-first by resolution tier: the engine evaluates `&&` left-to-right
    // and resolves each term at its cheapest tier — `scope` (addr, no IO) <
    // `label` (spec/`get_spec`) < `tree_output` (def/`get_def`). Freeze that
    // order in both source-set queries: `label` before `tree_output`.
    #[test]
    fn source_set_queries_order_label_before_tree_output() {
        let go_src = compute_pkg_src_addrs("pkg", &[])
            .unwrap()
            .into_iter()
            .find(|s| s.contains("label(go_src)"))
            .expect("go_src query present");
        assert!(
            go_src.find("label(go_src)").unwrap() < go_src.find("tree_output(").unwrap(),
            "go_src query must check label before tree_output: {go_src}"
        );

        let embed = compute_embed_src_addrs("pkg", &[])
            .unwrap()
            .into_iter()
            .find(|s| s.contains("label(go_embed_src)"))
            .expect("go_embed_src query present");
        assert!(
            embed.find("label(go_embed_src)").unwrap() < embed.find("tree_output(").unwrap(),
            "go_embed_src query must check label before tree_output: {embed}"
        );
    }

    /// A non-root package scopes its source query to its own subtree, so a generator
    /// in a sub-package is reachable. The root package does not: its subtree is the
    /// whole workspace, including the synthetic `//@heph/…` provider namespaces.
    #[test]
    fn default_scope_is_the_subtree_except_at_the_root() {
        assert_eq!(default_scope("app"), "//app/...");
        assert_eq!(
            default_scope("mgmt/go/cmd/exporter/rest"),
            "//mgmt/go/cmd/exporter/rest/..."
        );
        assert_eq!(default_scope(""), "//");
    }

    #[test]
    fn compute_pkg_src_addrs_excludes_go_embed_src_lane() {
        // Decoupling guarantee: `_golist`'s source set pulls the `go_src` lane but
        // never `go_embed_src`, so an expensive asset build (frontend bundle) can
        // never block `go list` / query / metadata.
        let addrs = compute_pkg_src_addrs("pkg", &[]).unwrap();
        assert!(
            addrs.iter().any(|s| s.contains("label(go_src)")),
            "go_src lane must be present in golist srcfiles: {addrs:?}"
        );
        assert!(
            !addrs.iter().any(|s| s.contains("go_embed_src")),
            "golist srcfiles must NOT reference the go_embed_src lane: {addrs:?}"
        );
    }

    // The `go_src`/`go_embed_src`/`go_test_data` labels are only ever carried by
    // buildfile-emitted codegen targets. Every one of these label queries must
    // exclude the `go` provider, so resolving them never spec-resolves the go
    // provider's own targets (which drags in the golist/std graph) just to reject
    // them on the label — and never re-enters `get_spec` for the very addr being
    // resolved.
    #[test]
    fn label_queries_exclude_the_go_provider() {
        let exclude = format!("{}=go", hplugin_query::pluginquery::EXCLUDE_PROVIDER_ARG);

        let src = compute_pkg_src_addrs("pkg", &[]).unwrap();
        let go_src = src
            .iter()
            .find(|s| s.contains("label(go_src)"))
            .expect("go_src query present");
        assert!(
            go_src.contains(&exclude),
            "go_src query must exclude the go provider: {go_src}"
        );

        let embed = compute_embed_src_addrs("pkg", &[]).unwrap();
        let go_embed = embed
            .iter()
            .find(|s| s.contains("label(go_embed_src)"))
            .expect("go_embed_src query present");
        assert!(
            go_embed.contains(&exclude),
            "go_embed_src query must exclude the go provider: {go_embed}"
        );

        let data = go_test_data_query_addr("pkg").format();
        assert!(
            data.contains(&exclude),
            "go_test_data query must exclude the go provider: {data}"
        );
    }

    // Regression: the root non-go glob (`**/*` minus `.go`) also captured
    // `go.mod`/`go.sum`, which the modfiles (`_go_mod`) lane already delivers
    // into `_golist` as `fs:file` deps. Both producing the same sandbox file is
    // an output collision, so the glob must exclude the module files.
    #[test]
    fn compute_pkg_src_addrs_glob_excludes_module_files() {
        let addrs = compute_pkg_src_addrs("", &[]).unwrap();
        let glob = addrs
            .iter()
            .find(|s| s.contains(":glob@"))
            .expect("non-go glob present in golist srcfiles");
        assert!(
            glob.contains("**/go.mod"),
            "glob must exclude go.mod: {glob}"
        );
        assert!(
            glob.contains("**/go.sum"),
            "glob must exclude go.sum: {glob}"
        );
    }

    // Regression: a BUILD file declaring only `go_codegen_deps` (no
    // `go_codegen_root=true`) on an ancestor package must still inject those
    // deps into a descendant `_golist` sandbox. Previously the deps lookup was
    // nested inside the root-marker check, so without the marker the codegen
    // target never ran and `_golist` for an empty (codegen-only) directory
    // hit NoGoFilesError.
    #[tokio::test]
    async fn golist_appends_codegen_deps_without_root_marker() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let mut state_map = HashMap::new();
        state_map.insert(
            "go_codegen_deps".to_string(),
            Value::List(vec![Value::String("//codegen:gen".to_string())]),
        );
        let state = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: state_map,
        };
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![state, host_variant_state()],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
                states: vec![host_variant_state()],
            }),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let srcfiles = extract_srcfiles(&resp);
        assert!(
            srcfiles.iter().any(|s| s == "//codegen:gen"),
            "go_codegen_deps must be appended even without go_codegen_root, got: {srcfiles:?}"
        );
    }
}
