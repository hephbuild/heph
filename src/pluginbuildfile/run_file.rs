use crate::engine::driver::TargetAddr;
use crate::engine::driver::sandbox::{Dep, Env, EnvValue, Mode, Sandbox, Tool};
use crate::hmemoizer::unwrap_arc_err;
use crate::htaddr;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::{
    TargetSpecValue, parse_map_string_string, parse_map_string_strings, parse_strings,
};
use crate::pluginbuildfile::provider::Provider;
use anyhow::Context;
use enclose::enclose;
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Arguments, Evaluator, FileLoader};
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::float::UnpackFloat;
use starlark::values::list::{AllocList, UnpackList};
use starlark::values::{UnpackValue, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

static GLOBALS: OnceLock<Globals> = OnceLock::new();

fn get_globals() -> &'static Globals {
    GLOBALS.get_or_init(|| {
        GlobalsBuilder::standard()
            .with(starlark_module)
            .with_namespace("heph", |b| {
                b.namespace("fs", heph_fs_module);
            })
            .build()
    })
}

#[derive(Debug, Clone)]
pub(crate) struct OnTargetPayload {
    pub name: String,
    pub driver: String,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
    pub config: HashMap<String, TargetSpecValue>,
}

impl Sandbox {
    fn from(m: TargetSpecValue, pkg: &PkgBuf) -> anyhow::Result<Self> {
        let m = match m {
            TargetSpecValue::Map(m) => m,
            TargetSpecValue::Null() => {
                return Ok(Default::default());
            }
            _ => anyhow::bail!("Expected map, got {:?}", m),
        };

        let mut m: HashMap<&str, &TargetSpecValue> =
            m.iter().map(|(k, v)| (k.as_str(), v)).collect();

        let mut sandbox = Self::default();

        if let Some(v) = m.remove("deps") {
            let parsed = parse_map_string_strings(v).with_context(|| "parse `deps`")?;
            for (i, (k, ss)) in parsed.iter().enumerate() {
                for s in ss {
                    sandbox.push_dep(Dep {
                        r#ref: TargetAddr::parse(s, pkg).with_context(|| "parse `deps`")?,
                        mode: Mode::None,
                        group: k.to_string(),
                        runtime: true,
                        hash: true,
                        id: format!("dep|{}|{}", k, i),
                    });
                }
            }
        }

        if let Some(v) = m.remove("env") {
            sandbox.env = parse_map_string_string(v)
                .with_context(|| "parse `env`")?
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        Env {
                            value: EnvValue::Literal(v),
                            hash: true,
                            append: false,
                            append_prefix: "".to_string(),
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
        }

        if let Some(v) = m.remove("tools") {
            let parsed = parse_map_string_strings(v).with_context(|| "parse `tools`")?;
            for (i, (k, ss)) in parsed.iter().enumerate() {
                for s in ss {
                    sandbox.push_tool(Tool {
                        r#ref: TargetAddr::parse(s, pkg).with_context(|| "parse `tools`")?,
                        group: k.to_string(),
                        hash: true,
                        id: format!("tool|{}|{}", k, i),
                    });
                }
            }
        }

        if let Some(v) = m.remove("pass_env") {
            for name in parse_strings(v).with_context(|| "parse `pass_env`")? {
                sandbox.env.insert(
                    name,
                    Env {
                        value: EnvValue::Pass,
                        hash: true,
                        append: false,
                        append_prefix: "".to_string(),
                    },
                );
            }
        }

        if let Some(v) = m.remove("runtime_pass_env") {
            for name in parse_strings(v).with_context(|| "parse `runtime_pass_env`")? {
                sandbox.env.insert(
                    name,
                    Env {
                        value: EnvValue::Pass,
                        hash: false,
                        append: false,
                        append_prefix: "".to_string(),
                    },
                );
            }
        }

        if !m.is_empty() {
            let unknown_keys: Vec<&str> = m.into_keys().collect();
            anyhow::bail!("unknown entries found: {:?}", unknown_keys)
        }

        Ok(sandbox)
    }
}

#[expect(dead_code, reason = "fields read via pattern matching or future use")]
#[derive(Debug, Clone)]
pub(crate) struct OnStatePayload {
    pub name: String,
    pub labels: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct RunResult {
    pub targets: Vec<OnTargetPayload>,
    pub states: Vec<OnStatePayload>,
    /// Frozen Starlark module for this BUILD file. Exposes top-level symbols so that
    /// other BUILD files can import them via `load("//pkg", "sym")`. Empty when the
    /// package has no BUILD file matching the configured patterns.
    pub module: FrozenModule,
}

fn empty_run_result() -> anyhow::Result<RunResult> {
    let module = Module::new().freeze().map_err(anyhow::Error::from)?;
    Ok(RunResult {
        targets: vec![],
        states: vec![],
        module,
    })
}

fn merge_pkg_results(parts: &[Arc<RunResult>]) -> anyhow::Result<RunResult> {
    let mut targets = vec![];
    let mut states = vec![];
    let module = Module::new();
    for part in parts {
        module
            .frozen_heap()
            .add_reference(part.module.frozen_heap());
        for name in part.module.names() {
            let name_str = name.as_str();
            // Skip underscore-prefixed (private) symbols — they're never re-exportable.
            if name_str.starts_with('_') {
                continue;
            }
            if let Some(owned) = part.module.get_option(name_str)? {
                let value = owned.owned_value(module.frozen_heap());
                module.set(name_str, value);
            }
        }
        targets.extend(part.targets.iter().cloned());
        states.extend(part.states.iter().cloned());
    }
    let frozen = module.freeze().map_err(anyhow::Error::from)?;
    Ok(RunResult {
        targets,
        states,
        module: frozen,
    })
}

#[expect(
    dead_code,
    reason = "fields accessed via downcast_ref in starlark evaluator context"
)]
#[derive(ProvidesStaticType)]
pub(crate) struct Extra<'a> {
    pub pkg: &'a str,
    pub root: &'a Path,
    pub on_state: Box<dyn Fn(OnStatePayload) -> anyhow::Result<()>>,
    pub on_target: Box<dyn Fn(OnTargetPayload) -> anyhow::Result<()>>,
}

// Unsupported starlark value types (not str/bool/int/float/list/dict) are a programming error
#[expect(
    clippy::panic,
    reason = "caller must only pass supported starlark value types; any other type is a programming error"
)]
fn starlark_to_rust(v: &Value) -> TargetSpecValue {
    if v.is_none() {
        return TargetSpecValue::Null();
    }

    if let Some(s) = v.unpack_str() {
        return TargetSpecValue::String(s.to_string());
    }

    if let Some(b) = v.unpack_bool() {
        return TargetSpecValue::Bool(b);
    }

    if let Some(i) = v.unpack_i32() {
        return TargetSpecValue::Int(i as i64);
    }

    if let Ok(Some(UnpackFloat(f))) = UnpackFloat::unpack_value(*v) {
        return TargetSpecValue::Float(f);
    }

    if let Ok(Some(l)) = UnpackList::<Value>::unpack_value(*v) {
        return TargetSpecValue::List(l.items.iter().map(starlark_to_rust).collect());
    }

    if let Some(d) = DictRef::from_value(*v) {
        let map = d
            .iter()
            .filter_map(|(k, val)| {
                k.unpack_str()
                    .map(|s| (s.to_string(), starlark_to_rust(&val)))
            })
            .collect();
        return TargetSpecValue::Map(map);
    }

    panic!(
        "starlark_to_rust: Unsupported starlark value type: {}",
        v.get_type()
    );
}

/// Returns `path` prefixed with the current package (e.g. `"src/main.rs"` from pkg
/// `"foo/bar"` becomes `"foo/bar/src/main.rs"`). If `abs` is true, or the current
/// package is empty (workspace root), returns `path` unchanged.
fn resolve_fs_path(eval: &Evaluator, path: &str, abs: bool) -> String {
    if abs {
        return path.to_string();
    }
    let extra = eval
        .extra
        .expect("evaluator extra must be set")
        .downcast_ref::<Extra>()
        .expect("evaluator extra must be of type Extra");
    if extra.pkg.is_empty() {
        path.to_string()
    } else {
        format!("{}/{}", extra.pkg, path)
    }
}

#[starlark_module]
fn starlark_module(builder: &mut GlobalsBuilder) {
    fn target<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        args: &Arguments<'v, '_>,
    ) -> starlark::Result<String> {
        args.no_positional_args(eval.heap())?;
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling target()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");

        let m = args.names_map()?;

        let mut name = String::new();
        let mut driver = String::new();
        let mut labels: Vec<String> = vec![];
        let mut transitive: Sandbox = Default::default();
        let config = m
            .iter()
            .map(|e| -> anyhow::Result<Option<(String, TargetSpecValue)>> {
                match e.0.as_str() {
                    "name" => {
                        if let Some(s) = e.1.unpack_str() {
                            name = s.to_string();
                        }
                        Ok(None)
                    }
                    "driver" => {
                        if let Some(s) = e.1.unpack_str() {
                            driver = s.to_string();
                        }
                        Ok(None)
                    }
                    "labels" => {
                        if let Some(s) = e.1.unpack_str() {
                            labels = vec![s.to_string()];
                        } else {
                            labels = UnpackList::<String>::unpack_value_err(*e.1)
                                .map_err(|e| anyhow::anyhow!("{e}"))?
                                .items;
                        }
                        Ok(None)
                    }
                    "transitive" => {
                        transitive = Sandbox::from(starlark_to_rust(e.1), &PkgBuf::from(extra.pkg))
                            .with_context(|| "transitive")?;
                        Ok(None)
                    }
                    _ => Ok(Some((e.0.as_str().to_string(), starlark_to_rust(e.1)))),
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<HashMap<String, TargetSpecValue>>();

        if name.is_empty() {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "target name cannot be empty"
            )));
        }
        if driver.is_empty() {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "target driver cannot be empty"
            )));
        }

        let p = OnTargetPayload {
            name: name.clone(),
            driver,
            labels,
            transitive,
            config,
        };

        (extra.on_target)(p)?;

        Ok(htaddr::Addr::new(PkgBuf::from(extra.pkg), name, Default::default()).format())
    }

    fn file<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        path: &str,
        #[starlark(require = named, default = false)] abs: bool,
    ) -> starlark::Result<String> {
        let resolved = resolve_fs_path(eval, path, abs);
        Ok(crate::pluginfs::file_addr(&resolved).format())
    }

    fn glob<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        pattern: &str,
        exclude: Option<Value<'v>>,
        #[starlark(require = named, default = false)] abs: bool,
    ) -> starlark::Result<String> {
        let resolved = resolve_fs_path(eval, pattern, abs);
        let excludes: Vec<String> = match exclude {
            Some(v) => {
                if let Some(s) = v.unpack_str() {
                    vec![s.to_string()]
                } else {
                    UnpackList::<String>::unpack_value_err(v)
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                        .items
                }
            }
            None => vec![],
        };
        let excludes_ref: Vec<&str> = excludes.iter().map(String::as_str).collect();
        Ok(crate::pluginfs::glob_addr(&resolved, &excludes_ref).format())
    }

    fn r#struct<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        args: &Arguments<'v, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let m = args.names_map()?;
        let pairs: Vec<(&str, Value<'v>)> = m.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        Ok(eval.heap().alloc(starlark::values::dict::AllocDict(pairs)))
    }

    // TODO: wire provider_state into the engine's state registry (currently a no-op so
    // BUILD files can declare it without errors).
    fn provider_state<'v>(
        _eval: &mut Evaluator<'v, '_, '_>,
        _args: &Arguments<'v, '_>,
    ) -> starlark::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    fn get_pkg<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<String> {
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling get_pkg()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");
        Ok(extra.pkg.to_string())
    }
}

/// Built-in exclude patterns for `heph.fs.glob` — matches `pluginfs` driver behavior so
/// that BUILD-time glob expansion stays consistent with what the driver would see.
const HEPH_FS_GLOB_BUILTIN_EXCLUDES: &[&str] = &[".git/**/*", ".heph3/**/*", ".heph/**/*"];

#[starlark_module]
fn heph_fs_module(builder: &mut GlobalsBuilder) {
    /// Expand `pattern` (resolved relative to the current package) against the workspace
    /// filesystem and return matched file paths relative to the current package. Skips
    /// directories and built-in excludes (`.git`, `.heph`, `.heph3`).
    fn glob<'v>(eval: &mut Evaluator<'v, '_, '_>, pattern: &str) -> starlark::Result<Value<'v>> {
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling heph.fs.glob()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");

        let resolved = if extra.pkg.is_empty() {
            pattern.to_string()
        } else {
            format!("{}/{}", extra.pkg, pattern)
        };

        let abs_pattern = extra.root.join(&resolved);
        let abs_pattern_str = abs_pattern.to_str().ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "heph.fs.glob: pattern path is not valid UTF-8"
            ))
        })?;

        let excludes: Vec<glob::Pattern> = HEPH_FS_GLOB_BUILTIN_EXCLUDES
            .iter()
            .map(|s| glob::Pattern::new(s).expect("built-in exclude pattern must compile"))
            .collect();

        let pkg_prefix = (!extra.pkg.is_empty()).then(|| Path::new(extra.pkg));

        let entries = glob::glob(abs_pattern_str)
            .map_err(|e| starlark::Error::new_other(anyhow::Error::from(e)))?;

        let mut paths: Vec<String> = Vec::new();
        for entry in entries {
            let entry_path =
                entry.map_err(|e| starlark::Error::new_other(anyhow::Error::from(e)))?;

            if entry_path.is_dir() {
                continue;
            }

            let rel = entry_path.strip_prefix(extra.root).map_err(|e| {
                starlark::Error::new_other(anyhow::Error::from(e).context(format!(
                    "heph.fs.glob: entry `{}` is outside workspace root `{}`",
                    entry_path.display(),
                    extra.root.display()
                )))
            })?;

            let pkg_rel = pkg_prefix
                .and_then(|prefix| rel.strip_prefix(prefix).ok())
                .unwrap_or(rel);

            // Excludes are matched against the pkg-relative path so that a `.git`
            // directory inside the package is filtered out regardless of how deep
            // the package itself sits under the workspace root.
            if excludes.iter().any(|p| p.matches_path(pkg_rel)) {
                continue;
            }

            let s = pkg_rel.to_str().ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "heph.fs.glob: entry path is not valid UTF-8: {}",
                    pkg_rel.display()
                ))
            })?;

            paths.push(s.to_string());
        }

        paths.sort();
        Ok(eval.heap().alloc(AllocList(paths)))
    }
}

impl Provider {
    pub(crate) async fn run_pkg(&self, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        let key = pkg.to_string();
        let root = self.root.clone();
        let patterns = self.build_file_patterns.clone();
        let file_cache = self.file_cache.clone();
        let dir_cache = self.dir_cache.clone();
        self.pkg_cache
            .once(
                key.clone(),
                enclose!((key) move || async move {
                    tokio::task::spawn_blocking(move || -> anyhow::Result<Arc<RunResult>> {
                        let loader =
                            BuildFileLoader::new(root, patterns, file_cache, dir_cache);
                        loader
                            .load_pkg(&key)
                            .with_context(|| format!("pkg: `{}`", key))
                    })
                    .await
                    .context("run_pkg blocking task panicked")?
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }
}

/// Resolves `load(...)` paths against the workspace root and recursively evaluates
/// referenced BUILD files. Shared caches ensure a file/dir is parsed at most once
/// even when reached via different load paths or via top-level `run_pkg`.
pub(crate) struct BuildFileLoader {
    root: PathBuf,
    patterns: Vec<glob::Pattern>,
    file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    /// Files/dirs currently being evaluated on this call chain — guards against `load()` cycles.
    in_flight: Mutex<HashSet<PathBuf>>,
}

impl BuildFileLoader {
    fn new(
        root: PathBuf,
        patterns: Vec<glob::Pattern>,
        file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
        dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    ) -> Self {
        Self {
            root,
            patterns,
            file_cache,
            dir_cache,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Top-level entry: evaluate every BUILD file in `pkg`'s directory matching the
    /// configured patterns and merge their targets/states/symbols. Returns an empty
    /// result if the directory is missing or has no matching file — this is the path
    /// used by `Provider::list`/`get`/`probe`, where unknown packages should surface
    /// as empty/`NotFound` rather than hard errors. Use `load_dir` directly (via the
    /// `FileLoader` impl) for the `load(...)` path, which is strict.
    fn load_pkg(&self, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        let dir = self.root.join(pkg);
        if !dir.is_dir() {
            return Ok(Arc::new(empty_run_result()?));
        }
        let files = find_build_files(&dir, &self.patterns)?;
        if files.is_empty() {
            return Ok(Arc::new(empty_run_result()?));
        }
        self.load_dir(&dir, pkg)
    }

    fn load_dir(&self, dir: &Path, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        if let Some(cached) = self
            .dir_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("dir_cache lock poisoned"))?
            .get(dir)
        {
            return Ok(cached.clone());
        }

        let files = find_build_files(dir, &self.patterns)?;
        if files.is_empty() {
            let pats: Vec<&str> = self.patterns.iter().map(glob::Pattern::as_str).collect();
            anyhow::bail!(
                "no BUILD file in {} matching patterns {:?}",
                dir.display(),
                pats
            );
        }

        // Cycle guard on the dir entry itself so that load("//a") -> load("//a") is caught
        // even before we recurse into the same files.
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .map_err(|_e| anyhow::anyhow!("in_flight lock poisoned"))?;
            if !in_flight.insert(dir.to_path_buf()) {
                anyhow::bail!("load() cycle detected at {}", dir.display());
            }
        }
        let _guard = InFlightGuard {
            set: &self.in_flight,
            path: dir.to_path_buf(),
        };

        let mut parts = Vec::with_capacity(files.len());
        for file in &files {
            parts.push(
                self.load_file(file, pkg)
                    .with_context(|| format!("file: {}", file.display()))?,
            );
        }

        let merged = Arc::new(merge_pkg_results(&parts)?);
        self.dir_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("dir_cache lock poisoned"))?
            .insert(dir.to_path_buf(), merged.clone());
        Ok(merged)
    }

    fn load_file(&self, file_path: &Path, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        if let Some(cached) = self
            .file_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("file_cache lock poisoned"))?
            .get(file_path)
        {
            return Ok(cached.clone());
        }

        {
            let mut in_flight = self
                .in_flight
                .lock()
                .map_err(|_e| anyhow::anyhow!("in_flight lock poisoned"))?;
            if !in_flight.insert(file_path.to_path_buf()) {
                anyhow::bail!("load() cycle detected at {}", file_path.display());
            }
        }
        let _guard = InFlightGuard {
            set: &self.in_flight,
            path: file_path.to_path_buf(),
        };

        let result = Arc::new(
            eval_file(file_path, pkg, self)
                .with_context(|| format!("evaluating {}", file_path.display()))?,
        );
        self.file_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("file_cache lock poisoned"))?
            .insert(file_path.to_path_buf(), result.clone());
        Ok(result)
    }
}

struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<PathBuf>>,
    path: PathBuf,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.path);
        }
    }
}

/// Per-eval [`FileLoader`] view that knows which package the calling BUILD file lives in,
/// so `load("./foo.BUILD")` and `load("../other/foo.BUILD")` resolve relative to that
/// package rather than the workspace root.
struct ScopedLoader<'a> {
    inner: &'a BuildFileLoader,
    current_pkg: &'a str,
}

impl FileLoader for ScopedLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        self.inner
            .load_resolved(path, self.current_pkg)
            .map_err(starlark::Error::new_other)
    }
}

impl BuildFileLoader {
    fn load_resolved(&self, path: &str, current_pkg: &str) -> anyhow::Result<FrozenModule> {
        let candidate = resolve_load_target(&self.root, current_pkg, path)?;
        let meta = std::fs::metadata(&candidate)
            .with_context(|| format!("stat load path {}", candidate.display()))?;
        let result = if meta.is_file() {
            let pkg = candidate
                .parent()
                .and_then(|p| p.strip_prefix(&self.root).ok())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            self.load_file(&candidate, &pkg)?
        } else if meta.is_dir() {
            let pkg = candidate
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            self.load_dir(&candidate, &pkg)?
        } else {
            anyhow::bail!(
                "load() path {} is neither file nor directory",
                candidate.display()
            )
        };
        Ok(result.module.clone())
    }
}

/// Resolve a `load()` path argument to an absolute (logically normalized) filesystem path.
///
/// Accepts:
///   `//pkg/...`     absolute, relative to workspace root
///   `./rel/...`     relative to `current_pkg`'s directory
///   `../rel/...`    relative to `current_pkg`'s directory (walks up via `..`)
fn resolve_load_target(root: &Path, current_pkg: &str, path: &str) -> anyhow::Result<PathBuf> {
    let raw = if let Some(rel) = path.strip_prefix("//") {
        if rel.is_empty() {
            anyhow::bail!("load() path must not be empty after `//`");
        }
        root.join(rel)
    } else if path.starts_with("./") || path.starts_with("../") {
        root.join(current_pkg).join(path)
    } else {
        anyhow::bail!("load() path must start with `//`, `./`, or `../`, got `{path}`");
    };
    Ok(normalize_path(&raw))
}

/// Logically collapse `.` and `..` components in an absolute path without touching the
/// filesystem. Used so cache keys for `//a/sub` and `./sub` (from `a/BUILD`) match.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<std::path::Component> = Vec::with_capacity(path.components().count());
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(
                    out.last(),
                    Some(std::path::Component::Normal(_)) | Some(std::path::Component::CurDir)
                ) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().map(|c| c.as_os_str()).collect()
}

/// Enumerate every file in `dir` whose name matches any of `patterns`.
/// Result is sorted for deterministic ordering across runs. Propagates IO errors,
/// including "directory does not exist".
fn find_build_files(dir: &Path, patterns: &[glob::Pattern]) -> anyhow::Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|f| f.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| patterns.iter().any(|p| p.matches(n)))
        .collect();
    names.sort();
    Ok(names.into_iter().map(|n| dir.join(n)).collect())
}

fn eval_file(path: &Path, pkg: &str, loader: &BuildFileLoader) -> anyhow::Result<RunResult> {
    let ast: AstModule =
        AstModule::parse_file(path, &Dialect::Extended).map_err(|e| anyhow::anyhow!(e))?;

    let globals = get_globals();

    let module = Module::new();

    let targets = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let states = std::rc::Rc::new(std::cell::RefCell::new(vec![]));

    {
        let extra = Extra {
            pkg,
            root: &loader.root,
            on_target: {
                let targets = targets.clone();
                Box::new(move |p| {
                    targets.borrow_mut().push(p);

                    Ok(())
                })
            },
            on_state: {
                let states = states.clone();
                Box::new(move |p| {
                    states.borrow_mut().push(p);

                    Ok(())
                })
            },
        };
        let scoped = ScopedLoader {
            inner: loader,
            current_pkg: pkg,
        };
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&extra);
        eval.set_loader(&scoped);

        eval.eval_module(ast, globals)
            .map_err(starlark::Error::into_anyhow)?;
    }

    let targets = std::rc::Rc::try_unwrap(targets)
        .map_err(|_rc| anyhow::anyhow!("targets Rc still has outstanding references after eval"))?
        .into_inner();
    let states = std::rc::Rc::try_unwrap(states)
        .map_err(|_rc| anyhow::anyhow!("states Rc still has outstanding references after eval"))?
        .into_inner();

    let frozen = module
        .freeze()
        .map_err(anyhow::Error::from)
        .context("freezing starlark module")?;

    Ok(RunResult {
        targets,
        states,
        module: frozen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn run_pkg_blocking(provider: &Provider, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        let loader = BuildFileLoader::new(
            provider.root.clone(),
            provider.build_file_patterns.clone(),
            provider.file_cache.clone(),
            provider.dir_cache.clone(),
        );
        loader.load_pkg(pkg)
    }

    #[test]
    fn test_run_file_inner() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "mytarget",
    driver = "mydriver",
    labels = ["label1", "label2"],
    config_str = "hello",
    config_int = 42,
    config_bool = True,
    config_float = 1.5,
    config_list = ["a", 1],
)
"#;
        let filename = "BUILD".to_string();
        fs::write(pkg_path.join(&filename), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let result = run_pkg_blocking(&provider, &pkg_name).unwrap();

        assert_eq!(result.targets.len(), 1);
        let target = &result.targets[0];
        assert_eq!(target.name, "mytarget");
        assert_eq!(target.driver, "mydriver");
        assert_eq!(
            target.labels,
            vec!["label1".to_string(), "label2".to_string()]
        );

        if let Some(TargetSpecValue::String(s)) = target.config.get("config_str") {
            assert_eq!(s, "hello");
        } else {
            panic!(
                "Expected string for config_str, got {:?}",
                target.config.get("config_str")
            );
        }

        if let Some(TargetSpecValue::Int(i)) = target.config.get("config_int") {
            assert_eq!(*i, 42);
        } else {
            panic!(
                "Expected int for config_int, got {:?}",
                target.config.get("config_int")
            );
        }

        if let Some(TargetSpecValue::Bool(b)) = target.config.get("config_bool") {
            assert!(*b);
        } else {
            panic!(
                "Expected bool for config_bool, got {:?}",
                target.config.get("config_bool")
            );
        }

        if let Some(TargetSpecValue::Float(f)) = target.config.get("config_float") {
            assert_eq!(*f, 1.5);
        } else {
            panic!(
                "Expected float for config_float, got {:?}",
                target.config.get("config_float")
            );
        }

        if let Some(TargetSpecValue::List(l)) = target.config.get("config_list") {
            assert_eq!(l.len(), 2);
            if let TargetSpecValue::String(s) = &l[0] {
                assert_eq!(s, "a");
            } else {
                panic!("Expected string in list");
            }
            if let TargetSpecValue::Int(i) = &l[1] {
                assert_eq!(*i, 1);
            } else {
                panic!("Expected int in list");
            }
        } else {
            panic!("Expected list for config_list");
        }
    }

    fn make_provider(tmp_dir: &tempfile::TempDir) -> Provider {
        Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        }
    }

    fn run_transitive(build_content: &str) -> anyhow::Result<Sandbox> {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, pkg_name)?;
        Ok(result
            .targets
            .first()
            .map(|t| t.transitive.clone())
            .unwrap_or_default())
    }

    #[test]
    fn test_sandbox_empty_by_default() {
        let sandbox = run_transitive(r#"target(name="t", driver="d")"#).unwrap();
        assert!(sandbox.deps.is_empty());
        assert!(sandbox.tools.is_empty());
        assert!(sandbox.env.is_empty());
    }

    #[test]
    fn test_sandbox_null_transitive() {
        let sandbox = run_transitive(r#"target(name="t", driver="d", transitive=None)"#);
        // None is not a valid starlark value here; expect error or default
        // starlark doesn't have None by default in our globals, so this should error
        assert!(sandbox.is_err() || sandbox.unwrap().empty());
    }

    #[test]
    fn test_sandbox_deps_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"group1": ["//mypkg:other"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 1);
        let dep = &sandbox.deps[0];
        assert_eq!(dep.group, "group1");
        assert!(dep.runtime);
        assert!(dep.hash);
        assert_eq!(dep.r#ref.r#ref.name, "other");
        assert_eq!(dep.id, "dep|group1|0");
    }

    #[test]
    fn test_sandbox_multiple_deps() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"g": ["//mypkg:a", "//mypkg:b"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 2);
        let names: Vec<_> = sandbox
            .deps
            .iter()
            .map(|d| d.r#ref.r#ref.name.as_str())
            .collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn test_sandbox_tools_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "tools": {"toolgroup": ["//mypkg:mytool"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.tools.len(), 1);
        let tool = &sandbox.tools[0];
        assert_eq!(tool.group, "toolgroup");
        assert!(tool.hash);
        assert_eq!(tool.r#ref.r#ref.name, "mytool");
        assert_eq!(tool.id, "tool|toolgroup|0");
    }

    #[test]
    fn test_sandbox_env_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "env": {"MY_VAR": "my_value"},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.env.len(), 1);
        let env = sandbox.env.get("MY_VAR").unwrap();
        assert!(env.hash);
        assert!(!env.append);
        assert!(env.append_prefix.is_empty());
        match &env.value {
            EnvValue::Literal(s) => assert_eq!(s, "my_value"),
            _ => panic!("expected literal"),
        }
    }

    #[test]
    fn test_sandbox_all_fields() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"dg": ["//mypkg:dep1"]},
        "tools": {"tg": ["//mypkg:tool1"]},
        "env": {"K": "V"},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 1);
        assert_eq!(sandbox.tools.len(), 1);
        assert_eq!(sandbox.env.len(), 1);
    }

    #[test]
    fn test_sandbox_unknown_key_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "unknown_key": "value",
    },
)
"#;
        assert!(run_transitive(content).is_err());
    }

    #[test]
    fn test_sandbox_not_map_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = "bad",
)
"#;
        assert!(run_transitive(content).is_err());
    }

    #[tokio::test]
    async fn test_run_pkg_inner_multiple_patterns() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "mytarget",
    driver = "mydriver",
)
"#;
        let filename = "BUILD.heph".to_string();
        fs::write(pkg_path.join(&filename), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("BUILD.heph").unwrap(),
            ],
            ..Provider::default()
        };

        let result = provider.run_pkg(&pkg_name).await.unwrap();

        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "mytarget");
    }

    fn run_target_config(build_content: &str) -> HashMap<String, TargetSpecValue> {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, pkg_name).unwrap();
        result.targets.first().unwrap().config.clone()
    }

    #[test]
    fn test_starlark_file_pkg_relative_by_default() {
        // run_target_config runs in pkg "mypkg" — file("src/main.rs") should resolve
        // to "mypkg/src/main.rs".
        let content = r#"
target(
    name = "t",
    driver = "d",
    src = file("src/main.rs"),
)
"#;
        let config = run_target_config(content);
        let expected = crate::pluginfs::file_addr("mypkg/src/main.rs").format();
        match config.get("src") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected file addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_file_abs_skips_pkg_prefix() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    src = file("vendor/x.rs", abs = True),
)
"#;
        let config = run_target_config(content);
        let expected = crate::pluginfs::file_addr("vendor/x.rs").format();
        match config.get("src") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected file addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_glob_pkg_relative_by_default() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("src/**/*.rs"),
)
"#;
        let config = run_target_config(content);
        let expected = crate::pluginfs::glob_addr("mypkg/src/**/*.rs", &[]).format();
        match config.get("srcs") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_glob_with_exclude_pkg_relative() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.go", exclude = ["vendor/**", "gen/**"]),
)
"#;
        let config = run_target_config(content);
        let expected =
            crate::pluginfs::glob_addr("mypkg/**/*.go", &["vendor/**", "gen/**"]).format();
        match config.get("srcs") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_glob_abs_skips_pkg_prefix() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.rs", abs = True),
)
"#;
        let config = run_target_config(content);
        let expected = crate::pluginfs::glob_addr("**/*.rs", &[]).format();
        match config.get("srcs") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_load_package() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("BUILD"),
            r#"
MY_DRIVER = "shared_driver"
MY_LABELS = ["a", "b"]
"#,
        )
        .unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "MY_DRIVER", "MY_LABELS")
target(
    name = "t",
    driver = MY_DRIVER,
    labels = MY_LABELS,
)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "shared_driver");
        assert_eq!(result.targets[0].labels, vec!["a", "b"]);
    }

    #[test]
    fn test_load_file_explicit() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("macros.BUILD"),
            r#"
def make_name():
    return "from_macro"
"#,
        )
        .unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib/macros.BUILD", "make_name")
target(
    name = make_name(),
    driver = "d",
)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "from_macro");
    }

    #[test]
    fn test_load_missing_symbol_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("BUILD"), "X = 1\n").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//lib", "DOES_NOT_EXIST")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "app").is_err());
    }

    #[test]
    fn test_load_missing_package_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//nonexistent", "X")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "app").is_err());
    }

    #[test]
    fn test_load_cycle_detected() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(
            a.join("BUILD"),
            r#"load("//b", "Y")
X = Y
"#,
        )
        .unwrap();

        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(
            b.join("BUILD"),
            r#"load("//a", "X")
Y = X
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "a").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("cycle"), "{chain}");
    }

    #[test]
    fn test_pkg_with_multiple_build_files_merged() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let pkg = root.join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
target(name = "a", driver = "d")
SHARED_A = "from_a"
"#,
        )
        .unwrap();
        fs::write(
            pkg.join("more.BUILD"),
            r#"
target(name = "b", driver = "d")
SHARED_B = "from_b"
"#,
        )
        .unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("*.BUILD").unwrap(),
            ],
            ..Provider::default()
        };
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let names: Vec<&str> = result.targets.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"a"), "{names:?}");
        assert!(names.contains(&"b"), "{names:?}");
        assert_eq!(result.targets.len(), 2);
    }

    #[test]
    fn test_load_merged_symbols_from_multi_file_pkg() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("BUILD"), "FOO = \"foo\"\n").unwrap();
        fs::write(lib.join("more.BUILD"), "BAR = \"bar\"\n").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "FOO", "BAR")
target(name = "t", driver = FOO + BAR)
"#,
        )
        .unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("*.BUILD").unwrap(),
            ],
            ..Provider::default()
        };
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "foobar");
    }

    #[test]
    fn test_load_missing_dir_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//does/not/exist", "X")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "app").is_err());
    }

    #[test]
    fn test_load_dir_with_no_matching_pattern_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        // lib dir exists but contains no file matching the patterns.
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("README"), "not a build file").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//lib", "X")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "app").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("no BUILD file"), "{chain}");
    }

    #[test]
    fn test_run_pkg_missing_dir_returns_empty() {
        let tmp_dir = tempdir().unwrap();
        let provider = make_provider(&tmp_dir);
        let r = run_pkg_blocking(&provider, "nope").unwrap();
        assert!(r.targets.is_empty());
        assert!(r.states.is_empty());
    }

    #[test]
    fn test_run_pkg_dir_without_match_returns_empty() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("empty");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("README"), "").unwrap();
        let provider = make_provider(&tmp_dir);
        let r = run_pkg_blocking(&provider, "empty").unwrap();
        assert!(r.targets.is_empty());
    }

    #[test]
    fn test_stdlib_type_builtin() {
        let content = r#"
deps_kind = type(["a", "b"])
str_kind = type("x")
target(name = "t", driver = "d", deps_kind = deps_kind, str_kind = str_kind)
"#;
        let config = run_target_config(content);
        match config.get("deps_kind") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, "list"),
            other => panic!("expected list type string, got {other:?}"),
        }
        match config.get("str_kind") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, "string"),
            other => panic!("expected string type string, got {other:?}"),
        }
    }

    #[test]
    fn test_stdlib_type_used_in_conditional() {
        let content = r#"
def coerce(deps):
    if type(deps) != "list":
        return [deps]
    return deps

target(name = "t", driver = "d", deps = coerce("single"))
"#;
        let config = run_target_config(content);
        match config.get("deps") {
            Some(TargetSpecValue::List(l)) => {
                assert_eq!(l.len(), 1);
                match &l[0] {
                    TargetSpecValue::String(s) => assert_eq!(s, "single"),
                    other => panic!("expected string in list, got {other:?}"),
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn test_get_pkg_returns_current_pkg() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("some").join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", here = get_pkg())"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "some/pkg").unwrap();
        assert_eq!(result.targets.len(), 1);
        match result.targets[0].config.get("here") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, "some/pkg"),
            other => panic!("expected pkg string, got {other:?}"),
        }
    }

    #[test]
    fn test_get_pkg_in_loaded_file_reports_loader_pkg() {
        // load("//lib", ...) evaluates lib's BUILD under pkg "lib" — get_pkg() there
        // returns "lib", not the caller's package.
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("BUILD"), r#"WHERE = get_pkg()"#).unwrap();
        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "WHERE")
target(name = "t", driver = "d", loaded_from = WHERE, here = get_pkg())
"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        let cfg = &result.targets[0].config;
        match cfg.get("loaded_from") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, "lib"),
            other => panic!("expected lib, got {other:?}"),
        }
        match cfg.get("here") {
            Some(TargetSpecValue::String(s)) => assert_eq!(s, "app"),
            other => panic!("expected app, got {other:?}"),
        }
    }

    #[test]
    fn test_provider_state_noop() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
provider_state(name = "s", labels = ["a"])
target(name = "t", driver = "d")
"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "p").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert!(result.states.is_empty());
    }

    #[test]
    fn test_struct_builtin_returns_dict() {
        let content = r#"
s = struct(name = "n", driver = "d", count = 3)
target(
    name = s["name"],
    driver = s["driver"],
    cfg = s,
)
"#;
        let config = run_target_config(content);
        match config.get("cfg") {
            Some(TargetSpecValue::Map(m)) => {
                match m.get("name") {
                    Some(TargetSpecValue::String(s)) => assert_eq!(s, "n"),
                    other => panic!("expected name string, got {other:?}"),
                }
                match m.get("driver") {
                    Some(TargetSpecValue::String(s)) => assert_eq!(s, "d"),
                    other => panic!("expected driver string, got {other:?}"),
                }
                match m.get("count") {
                    Some(TargetSpecValue::Int(i)) => assert_eq!(*i, 3),
                    other => panic!("expected count int, got {other:?}"),
                }
            }
            other => panic!("expected dict, got {other:?}"),
        }
    }

    #[test]
    fn test_struct_builtin_rejects_positional() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
s = struct("positional")
target(name = "t", driver = "d")
"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "p").is_err());
    }

    #[test]
    fn test_load_relative_dot_file_same_pkg() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let pkg = root.join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("go.BUILD2"), "go_install = \"installed\"\n").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
load("./go.BUILD2", "go_install")
target(name = "t", driver = go_install)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "installed");
    }

    #[test]
    fn test_load_relative_parent_dir() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("shared.BUILD"), "FOO = \"shared\"\n").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("../lib/shared.BUILD", "FOO")
target(name = "t", driver = FOO)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "shared");
    }

    #[test]
    fn test_load_bare_path_rejected() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let pkg = root.join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("BUILD"), r#"load("foo.BUILD", "X")"#).unwrap();
        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "p").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("must start with"), "{chain}");
    }

    #[test]
    fn test_load_target_in_other_package_registers_there() {
        // Loading another package's BUILD file evaluates it; any target() calls
        // it makes register against THAT package, not the loader's package.
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("BUILD"),
            r#"
target(name = "t_in_lib", driver = "d")
SHARED = "s"
"#,
        )
        .unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "SHARED")
target(name = "t_in_app", driver = SHARED)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let app_res = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(app_res.targets.len(), 1);
        assert_eq!(app_res.targets[0].name, "t_in_app");
        assert_eq!(app_res.targets[0].driver, "s");

        // Lib was parsed via the load() and cached; re-querying lib should return
        // the targets defined there without re-eval.
        let lib_res = run_pkg_blocking(&provider, "lib").unwrap();
        assert_eq!(lib_res.targets.len(), 1);
        assert_eq!(lib_res.targets[0].name, "t_in_lib");
    }

    fn expect_string_list(v: Option<&TargetSpecValue>) -> Vec<String> {
        match v {
            Some(TargetSpecValue::List(l)) => l
                .iter()
                .map(|e| match e {
                    TargetSpecValue::String(s) => s.clone(),
                    other => panic!("expected string in list, got {other:?}"),
                })
                .collect(),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn test_heph_fs_glob_returns_pkg_relative_paths() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("src").join("a.yaml"), "").unwrap();
        fs::write(pkg.join("src").join("b.yaml"), "").unwrap();
        fs::write(pkg.join("src").join("c.txt"), "").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("src/*.yaml"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let mut srcs = expect_string_list(result.targets[0].config.get("srcs"));
        srcs.sort();
        assert_eq!(
            srcs,
            vec!["src/a.yaml".to_string(), "src/b.yaml".to_string()]
        );
    }

    #[test]
    fn test_heph_fs_glob_no_matches_returns_empty_list() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("src/*.yaml"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let srcs = expect_string_list(result.targets[0].config.get("srcs"));
        assert!(srcs.is_empty(), "expected empty list, got {srcs:?}");
    }

    #[test]
    fn test_heph_fs_glob_skips_directories() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(pkg.join("nested")).unwrap();
        fs::write(pkg.join("a.txt"), "").unwrap();
        fs::write(pkg.join("nested").join("b.txt"), "").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("*"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let srcs = expect_string_list(result.targets[0].config.get("srcs"));
        // BUILD + a.txt; nested/ is a directory and must be filtered out.
        assert!(srcs.contains(&"a.txt".to_string()), "{srcs:?}");
        assert!(srcs.contains(&"BUILD".to_string()), "{srcs:?}");
        assert!(!srcs.iter().any(|s| s == "nested"), "{srcs:?}");
    }

    #[test]
    fn test_heph_fs_glob_excludes_builtin_dirs() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        let git = pkg.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "").unwrap();
        fs::write(pkg.join("main.rs"), "").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("**/*"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let srcs = expect_string_list(result.targets[0].config.get("srcs"));
        assert!(
            !srcs.iter().any(|s| s.starts_with(".git")),
            ".git entries should be excluded: {srcs:?}"
        );
        assert!(srcs.contains(&"main.rs".to_string()), "{srcs:?}");
    }

    #[test]
    fn test_heph_fs_glob_at_workspace_root_returns_unprefixed() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        fs::write(root.join("a.yaml"), "").unwrap();
        fs::write(root.join("b.yaml"), "").unwrap();
        fs::write(
            root.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("*.yaml"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "").unwrap();
        let mut srcs = expect_string_list(result.targets[0].config.get("srcs"));
        srcs.sort();
        assert_eq!(srcs, vec!["a.yaml".to_string(), "b.yaml".to_string()]);
    }
}
