//! Deferred values: a driver option whose value is computed by another target.
//!
//! A driver option's value has to be a literal in the BUILD file. But the value's
//! owner is usually somewhere else — a role ARN in the Terraform that created the
//! role, a registry per environment in a platform team's repo, a version in a
//! release process. Copying it in makes a second source of truth that someone has
//! to keep agreeing with the first, purely to satisfy heph.
//!
//! ```python
//! role = "${read://infra/aws:role-arn}"
//! ```
//!
//! # Why this is not what every other build system does
//!
//! Gradle, Bazel, Buck2, Pants and Nix each shipped an answer, and they agree in
//! structure because they made the same choice: **the value crosses the boundary
//! as a bare string, and the thing that produced it never becomes a node in the
//! graph.** Three things follow mechanically. Change can only be detected by
//! re-running the producer eagerly on every invocation — which is why every one of
//! those systems documents some form of *"only use fast commands here"*. The cache
//! key can only be all-or-nothing: hash the string and over-invalidate, or hide it
//! and sanction staleness. And provenance ends at the string, so "why did this
//! rebuild?" bottoms out at `environment variable 'X' has changed`.
//!
//! Here the producer **is** a node. It declares its own inputs, so it re-runs when
//! the thing that defines the value changes and not otherwise; the consumer's key
//! derives from the Terraform state rather than from the ARN string; and
//! `heph inspect deps` can say which field wanted it.
//!
//! # The two moments
//!
//! At **parse**, the reference becomes an edge: the host walks the raw config,
//! finds every `${read://…}` at any nesting depth, and appends an [`Input`] with
//! `hashed: true, runtime: false`. At **run**, the bytes arrive: the host reads the
//! producer's output and hands the substituted value to the driver.
//!
//! The def hash covers the **unresolved** reference — that is what keeps
//! `heph query` and `heph inspect def` from triggering a build — while the
//! producer's content still reaches `hashin`, because `hashin` is the def hash plus
//! the hashouts of hashed inputs. The correct behaviour is a structural consequence
//! of where the work happens rather than a rule anyone has to follow.
//!
//! # Why the host and not the driver
//!
//! An earlier shape left three obligations on the plugin author: type the field,
//! collect its edges at parse, substitute at run. Both "remembers" fail *silently*
//! — a missed edge means the producer never builds and the value never enters the
//! key; a missed substitution means `"${read://infra:role-arn}"` is used as the
//! ARN. That is the exact bug class this exists to remove, reintroduced one layer
//! down. So the walk is host infrastructure over data every plugin transport
//! already carries, and a driver's whole diff is one field type.
//!
//! # Two edge shapes, and why only one ships
//!
//! `${read://…}` is `(hashed, not staged)` — the `hash_deps`/`runner` cell. It is
//! deliberately **not** `runtime: true`: that would merge the *producer's*
//! transitive tools, deps and env into every consumer (`collect_transitive_deps`
//! filters on `i.runtime`) and stage the producer's file into the consumer's
//! sandbox. A credential target must not inherit Terraform's environment.
//!
//! `${src://…}` — the sandbox *path* of an artifact — is the other cell and is not
//! implemented here; nor is `${env:NAME}`. Both are reserved by the decoder, so a
//! BUILD file using one gets a clear refusal rather than a literal.

use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, InputMode, TargetDef};
use crate::engine::driver::{DeferredValue, TargetAddr};
use crate::engine::request_state::RequestState;
use anyhow::Context as _;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The *contents* of a producer's single output.
pub use hcore::template::READ_KIND as READ;

/// The sandbox *path* of a producer's artifact.
///
/// The other edge shape: `(hashed, staged)`, because a path is only meaningful
/// once the bytes are in the sandbox. Filled in by the managed-driver layer
/// rather than here, since only the layer that staged the artifact knows where
/// it landed — under FUSE the sandbox root is redirected after the host set it.
pub use hcore::template::SRC_KIND as SRC;

/// The largest a deferred value may be.
///
/// A value is one line — a role ARN, an image tag, a registry host. The cap is
/// what stops a one-line BUILD-file typo (a reference pointed at a Go binary)
/// from buffering the whole artifact in memory before failing.
const MAX_VALUE_BYTES: u64 = 64 * 1024;

/// `origin_id` prefix for a synthesized deferred edge.
///
/// Distinct from every dep prefix, so a deferred value can never collide with a
/// dep group, and so `heph inspect deps` reads as what it is: `option:` plus the
/// path through the config that wanted it.
pub const DEFERRED_ORIGIN_PREFIX: &str = "option";

/// Marks a `${src:}` edge as staged-but-not-transitively-merged.
///
/// `collect_transitive_deps` filters on `runtime`, and a `${src:}` input must be
/// `runtime: true` for the artifact to reach the sandbox at all — so the
/// exclusion needs its own mark. See [`inputs_for_labelled`].
pub const DEFERRED_SRC_ANNOTATION: &str = "heph.deferred.src";

/// One producer a reference names, and what the author asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Producer {
    /// [`READ`] or [`SRC`] — which decides the edge shape and who substitutes.
    pub kind: &'static str,
    /// The target, with any `|group` and `[filters]` the author wrote. Parsed by
    /// [`TargetAddr::parse`], the same function `deps` goes through, so a
    /// reference and a dep agree about what an address is.
    pub addr: TargetAddr,
}

/// One reference found in a target's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredRef {
    /// The field's whole text — a lone reference, or a template containing one.
    /// This is the key both ends agree on: a driver's `Deferred<T>` carries the
    /// text it decoded and knows nothing about where in the config it came from.
    pub raw: String,
    /// The producers this text names, in order of appearance.
    pub producers: Vec<Producer>,
    /// Where in the config it was found, e.g. `sources[0].present.env.AWS_ROLE_ARN`.
    /// Diagnostics only: it is what makes `heph inspect deps` able to say *which
    /// field* wanted an edge.
    pub path: String,
}

impl DeferredRef {
    /// Whether any producer here needs the sandbox, and therefore cannot be
    /// substituted until the artifact has been staged.
    pub fn needs_sandbox(&self) -> bool {
        self.producers.iter().any(|p| p.kind == SRC)
    }
}

/// Every deferred reference in a target's config, in a stable order.
///
/// Walks the raw `HashMap<String, Value>` the host already holds, so it needs to
/// know nothing about the driver's types — which matters, because references
/// nest: a credential's `sources = [heph.auth.oidc(present = {...})]` is a list of
/// maps of maps, and no flat per-field schema could describe a reference three
/// levels inside it.
///
/// Returns early for a config with no `${` anywhere, which is every target in
/// almost every workspace.
pub fn collect(
    config: &std::collections::HashMap<String, Value>,
    pkg: &PkgBuf,
) -> anyhow::Result<Vec<DeferredRef>> {
    // Nothing to find in the overwhelming majority of targets, and this is the
    // `get_def` path — every target, every run, cache hits included. So the scan
    // comes first and allocates nothing: no key sort, no path `String`, no
    // tokenizing, just one `memchr`-shaped pass per string leaf looking for `${`.
    if !config.values().any(any_ref) {
        return Ok(Vec::new());
    }

    let mut out: Vec<DeferredRef> = Vec::new();
    let mut keys: Vec<&String> = config.keys().collect();
    // Deterministic: a synthesized input's position must not depend on hash order.
    keys.sort_unstable();
    for k in keys {
        if let Some(v) = config.get(k)
            && any_ref(v)
        {
            walk(v, k, pkg, &mut out)?;
        }
    }
    // One edge per distinct text, however many fields carry it. Sorted first,
    // because `dedup_by` only removes *adjacent* duplicates — two fields far
    // apart in the config carrying the same reference would otherwise each get an
    // entry.
    out.sort_by(|a, b| a.raw.cmp(&b.raw));
    out.dedup_by(|a, b| a.raw == b.raw);
    Ok(out)
}

/// Whether any string anywhere under `v` contains a `${…}`.
///
/// Deliberately not "any *deferred* reference": this is the cheap pre-scan, and
/// `${` is rare enough in a config that paying the full tokenize on a hit is free,
/// while paying it on every `${OUT}` would not be.
fn any_ref(v: &Value) -> bool {
    match v {
        Value::String(s) => hcore::template::has_ref(s),
        Value::List(items) => items.iter().any(any_ref),
        Value::Map(m) => m.values().any(any_ref),
        _ => false,
    }
}

fn walk(v: &Value, path: &str, pkg: &PkgBuf, out: &mut Vec<DeferredRef>) -> anyhow::Result<()> {
    match v {
        Value::String(s) => {
            if let Some(r) = parse_refs(s, pkg, path)? {
                out.push(r);
            }
        }
        Value::List(items) => {
            for (i, item) in items.iter().enumerate() {
                // The path is a diagnostic, so it is built only for the branches
                // that actually contain a reference — a `deps` group of two
                // hundred globbed entries would otherwise cost two hundred
                // discarded `String`s per `get_def`, on every target, on a warm
                // build.
                if any_ref(item) {
                    walk(item, &format!("{path}[{i}]"), pkg, out)?;
                }
            }
        }
        Value::Map(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_unstable();
            for k in keys {
                if let Some(item) = m.get(k)
                    && any_ref(item)
                {
                    walk(item, &format!("{path}.{k}"), pkg, out)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The references in one string, or `None` when it holds none.
fn parse_refs(s: &str, pkg: &PkgBuf, path: &str) -> anyhow::Result<Option<DeferredRef>> {
    if !hcore::template::has_ref(s) {
        return Ok(None);
    }
    let mut producers = Vec::new();
    for piece in hcore::template::parse(s).with_context(|| format!("in `{path}`"))? {
        let hcore::template::Piece::Ref(r) = piece else {
            continue;
        };
        // Not ours unless the kind is one heph resolves *and* the argument is an
        // absolute address. `${FOO:-default}`, `${src:0:3}` and every other shell
        // construct has to survive being written in a deferrable field.
        if !hcore::template::claims(r.kind, r.arg, hcore::template::DEFERRED_KINDS) {
            continue;
        }
        let kind = match r.kind {
            Some(SRC) => SRC,
            _ => READ,
        };
        // `TargetAddr::parse`, not a bare address parse: `|group` and `[filters]`
        // are how every other dep narrows a multi-output producer, and a
        // reference has exactly the same need.
        let addr = TargetAddr::parse(r.arg, pkg).with_context(|| {
            format!(
                "`{}` in `{path}`: {:?} is not a target address",
                r.raw, r.arg
            )
        })?;
        producers.push(Producer { kind, addr });
    }
    if producers.is_empty() {
        return Ok(None);
    }
    Ok(Some(DeferredRef {
        raw: s.to_string(),
        producers,
        path: path.to_string(),
    }))
}

/// The inputs a set of references contributes to a def.
///
/// `(hashed, not staged)`. See the module docs for why `runtime: true` would be
/// wrong in two separate ways.
pub fn inputs_for(refs: &[DeferredRef]) -> Vec<Input> {
    inputs_for_labelled(refs, None)
}

/// [`inputs_for`], with the declaration the references came from.
///
/// `via` is set when the references were written somewhere other than this
/// target's own config — a credential declaration it names. Without it the edge
/// would land on the consumer carrying a path into a config the consumer does not
/// have (`option|sources[0].present.env.AWS_ROLE_ARN`), which is the opposite of
/// what an origin id is for; and two credentials with a reference at the same path
/// would give one consumer two inputs with identical ids.
/// The `origin_id` a reference's edge carries — and the key a pending `${src:}`
/// entry uses to find the staged input again. One function, because a mismatch
/// would mean the artifact was staged and the path still could not be found.
fn origin_id_for(r: &DeferredRef, p: &Producer, via: Option<&Addr>) -> String {
    match via {
        Some(v) => format!("{DEFERRED_ORIGIN_PREFIX}|{v}|{}|{}", p.kind, r.path),
        None => format!("{DEFERRED_ORIGIN_PREFIX}|{}|{}", p.kind, r.path),
    }
}

pub fn inputs_for_labelled(refs: &[DeferredRef], via: Option<&Addr>) -> Vec<Input> {
    // Keyed on the *cell* as well as the address. Deduping on the address alone
    // silently dropped one of them the moment two kinds existed: `collect` sorts
    // refs by their whole field text, so `"${read://a:b}"` sorts before
    // `"${src://a:b}"`, the `read` edge won, and the artifact was never staged —
    // with the winner depending on the surrounding literal characters.
    let mut seen: std::collections::BTreeSet<(String, bool)> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for r in refs {
        for p in &r.producers {
            let staged = p.kind == SRC;
            if !seen.insert((p.addr.to_string(), staged)) {
                continue;
            }
            let mut annotations = BTreeMap::new();
            if staged {
                // The third cell: staged like a `deps` edge, but excluded from
                // `collect_transitive_deps`. Writing `${src://tools:go}` asks for
                // a path; it does not ask to inherit the producer's `transitive`
                // environment — which `apply_transitive` would fold into this
                // target's *def hash* from `std::env::var`, so a consumer's key
                // would move with the host environment of a target it only
                // wanted a filename from.
                annotations.insert(DEFERRED_SRC_ANNOTATION.to_string(), String::new());
            }
            out.push(Input {
                r#ref: p.addr.clone(),
                mode: InputMode::Standard,
                origin_id: origin_id_for(r, p, via),
                annotations,
                hashed: true,
                runtime: staged,
            });
        }
    }
    out
}

impl Engine {
    /// Find every deferred reference in a target's config, refusing one the
    /// driver cannot understand.
    ///
    /// The gate is whole-driver and its default is `false`, which is what makes a
    /// mixed-version fleet safe: a plugin built before this feature says nothing
    /// about deferred values, so the host refuses a reference for it rather than
    /// letting its old `String` decoder read one as a literal and run the target
    /// with the reference text as the value.
    pub(crate) fn deferred_refs(
        &self,
        spec: &crate::engine::provider::TargetSpec,
        driver: &str,
    ) -> anyhow::Result<Vec<DeferredRef>> {
        // First, and before anything allocates: almost no target contains a
        // `${` at all, and this runs for every target on every build including
        // a full cache hit. One `memchr`-shaped pass per string leaf, no key
        // sort, no path `String`, no tokenizing — and, critically, no
        // `Driver::schema()`, which the derive rebuilds from scratch on every
        // call (17 `DriverField`s for `exec`, 82 allocations and ~1.6 µs). Asking
        // the schema before knowing there is anything to gate put that on the
        // warm path of every workspace, including those using none of this.
        if !spec.config.values().any(any_ref) {
            return Ok(Vec::new());
        }

        let refs =
            collect(&spec.config, &spec.addr.package).with_context(|| format!("{}", spec.addr))?;
        let accepts = self
            .drivers_by_name
            .get(driver)
            .is_some_and(|d| d.driver.schema().accepts_deferred);

        if refs.is_empty() {
            return Ok(refs);
        }
        if !accepts {
            let r = refs.first().map(|r| r.raw.as_str()).unwrap_or_default();
            anyhow::bail!(
                "{}: `{r}` is a deferred value, and the `{driver}` driver does not accept one. \
                 Either the field is not deferrable, or this plugin was built before deferred \
                 values existed — a plugin that predates them would decode the reference as a \
                 literal and run the target with that text as the value, so heph refuses it here \
                 instead. Rebuild the plugin against this heph, or move the value into a field \
                 that takes one",
                spec.addr
            );
        }
        Ok(refs)
    }

    /// Resolve every reference on a def, ready for `RunRequest.deferred`.
    ///
    /// Runs inside `execute`, so a cache hit resolves nothing — and the producers
    /// have already been built, because they are `hashed` inputs that `hashin`
    /// waited on. What is left here is reading bytes.
    pub(crate) async fn resolve_deferred(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        def: &TargetDef,
        refs: &[DeferredRef],
    ) -> anyhow::Result<BTreeMap<String, DeferredValue>> {
        if refs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let by_addr = self.deferred_values(rs, refs, &def.addr).await?;
        let mut values: BTreeMap<String, DeferredValue> = BTreeMap::new();
        for r in refs {
            // A field naming a `${src://…}` cannot be finished here: the path
            // exists only once the artifact is staged, and under FUSE the
            // sandbox root is redirected after this runs. Hand it on whole,
            // together with the `read` values it also names, so the completing
            // pass runs **once** over the author's own text — half-substituting
            // it here and finishing later would re-scan a producer's bytes,
            // which is the re-interpretation `hcore::template` is single-pass to
            // prevent.
            if r.needs_sandbox() {
                values.insert(
                    r.raw.clone(),
                    DeferredValue::NeedsSandbox {
                        reads: r
                            .producers
                            .iter()
                            .filter(|p| p.kind == READ)
                            .filter_map(|p| {
                                by_addr
                                    .get(&p.addr.to_string())
                                    .map(|v| (p.addr.to_string(), v.clone()))
                            })
                            .collect(),
                        srcs: r
                            .producers
                            .iter()
                            .filter(|p| p.kind == SRC)
                            .map(|p| (p.addr.to_string(), origin_id_for(r, p, None)))
                            .collect(),
                    },
                );
                continue;
            }
            // `substitute`, not `render`: a driver option is somebody else's text.
            // `echo tmp.$$` is the shell's PID idiom and `${src:0:3}` is bash, so
            // heph replaces the one construct it owns and reproduces the rest
            // byte for byte — otherwise a field's meaning would change depending
            // on whether a reference happened to appear elsewhere in it.
            //
            // Single-pass, over pieces of the *input*, so a producer whose output
            // contains `${` is never re-interpreted.
            let rendered = hcore::template::substitute(&r.raw, &[READ], |t| {
                let addr = TargetAddr::parse(t.arg, &def.addr.package)?;
                by_addr
                    .get(&addr.to_string())
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("no value for {addr}"))
            })?;
            values.insert(r.raw.clone(), DeferredValue::Ready(rendered));
        }
        Ok(values)
    }

    /// Every producer named by `refs`, resolved to its value, keyed by address.
    ///
    /// Separate from [`resolve_deferred`](Self::resolve_deferred) because the two
    /// consumers want different shapes: a driver is handed whole substituted
    /// fields, while the credential presentation substitutes token by token into
    /// templates the host itself renders.
    pub(crate) async fn deferred_values(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        refs: &[DeferredRef],
        consumer: &Addr,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut by_addr: BTreeMap<String, String> = BTreeMap::new();
        for r in refs {
            for p in r.producers.iter().filter(|p| p.kind == READ) {
                let key = p.addr.to_string();
                if by_addr.contains_key(&key) {
                    continue;
                }
                let value = self
                    .read_deferred_value_cached(rs, &p.addr)
                    .await
                    .with_context(|| format!("{consumer}: `{}` in `{}`", r.raw, r.path))?;
                by_addr.insert(key, value);
            }
        }
        Ok(by_addr)
    }

    /// The contents of a producer's single output, read at most once per content.
    ///
    /// Keyed on the producer's `hashin` and memoized on the engine, following
    /// `execrunner_host`'s rule: *cache the derived value, never the resolution*.
    /// The shape this feature invites is fan-out — one `//infra:registry`, every
    /// image target — and `result_addr` memoizes the *build* but not the artifact
    /// walk, so without this each consumer re-walks and re-reads the tar.
    ///
    /// Through the memoizer rather than a bare map so two concurrent misses
    /// single-flight rather than both doing the read.
    async fn read_deferred_value_cached(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &TargetAddr,
    ) -> anyhow::Result<String> {
        // The producer's own `hashin` identifies its content: two consumers of one
        // producer share a key, and a producer whose inputs moved does not.
        let meta = Arc::clone(self).meta(rs.clone(), &addr.r#ref).await?;
        self.deferred_values
            .once(
                // The producer's `hashin` plus the group, because `|out` and
                // `|meta` on one producer are two different values.
                match &addr.output {
                    Some(g) => format!("{}|{g}", meta.hashin),
                    None => meta.hashin,
                },
                enclose::enclose!((self => engine, rs, addr) move || async move {
                    engine.read_deferred_value(&rs, &addr).await
                }),
            )
            .await
            .map_err(hcore::hmemoizer::unwrap_arc_err)
    }

    /// The contents of a producer's single output.
    ///
    /// Read through `walk`, not `reader`: an artifact is a packed stream, so
    /// `reader` hands back the tar rather than the file — which arrives as a value
    /// full of header bytes and fails at `execve` with "nul byte in provided
    /// data", a very long way from where the mistake was.
    async fn read_deferred_value(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &TargetAddr,
    ) -> anyhow::Result<String> {
        use hcore::hartifactcontent::WalkEntryKind;

        let res = Arc::clone(self)
            .result_addr(
                rs.clone(),
                &addr.r#ref,
                // `|group` narrows a multi-output producer exactly as it does on
                // a dep, so a value can be read from a target that also emits
                // something else.
                match &addr.output {
                    Some(g) => crate::engine::OutputMatcher::Exact(vec![g.clone()]),
                    None => crate::engine::OutputMatcher::All,
                },
                &crate::engine::ResultOptions::default(),
            )
            .await?;

        // Counted before anything is read. A reference pointed at an ordinary
        // build target — a binary, an OCI layer — would otherwise buffer its whole
        // output in memory and only then discover there is no single value, which
        // makes the *error* path the expensive one.
        let mut names: Vec<String> = Vec::new();
        for artifact in &res.artifacts {
            for path in artifact.entry_paths()? {
                names.push(path.to_string_lossy().into_owned());
            }
        }
        if names.len() != 1 {
            names.sort();
            anyhow::bail!(
                "{addr} produces {} files ({}), so there is no single value to read. A value \
                 producer emits one file — split the target, or narrow its `out`",
                names.len(),
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
        }

        let mut body = Vec::new();
        for artifact in &res.artifacts {
            for entry in artifact.walk()? {
                let entry = entry?;
                let WalkEntryKind::File { mut data, .. } = entry.kind else {
                    continue;
                };
                let mut capped = std::io::Read::take(&mut data, MAX_VALUE_BYTES + 1);
                let read = std::io::copy(&mut capped, &mut body)
                    .with_context(|| format!("read {addr}'s output"))?;
                if read > MAX_VALUE_BYTES {
                    anyhow::bail!(
                        "{addr}'s output is larger than {MAX_VALUE_BYTES} bytes, which is not a \
                         value. A deferred value is one line — a role, a tag, a registry"
                    );
                }
            }
        }

        let text = String::from_utf8(body)
            .with_context(|| format!("{addr}'s output is not valid UTF-8"))?;
        // Surrounding whitespace is never part of the value. `echo`,
        // `printf '%s\n'` and `terraform output` all end with a newline; a file
        // committed on Windows ends with `\r\n`; a `yq`/`jq` pipeline can leave a
        // trailing space; and an author who indents a heredoc leaves leading
        // ones. None of that is a role ARN, an image tag or a registry host, and
        // trimming it here is the difference between a build that works and one
        // that fails inside somebody else's SDK with the value quoted back
        // looking correct.
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!(
                "{addr}'s output is empty. A silently-empty value is worse than a stopped build: \
                 it would reach the tool as an empty role, tag or registry"
            );
        }
        if text.contains('\n') {
            anyhow::bail!(
                "{addr}'s output has more than one line, and heph will not guess which one is the \
                 value. Make the producer emit one line"
            );
        }
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn pkg() -> PkgBuf {
        PkgBuf::from("auth")
    }

    /// The gate has to come before `Driver::schema()`, and this is the only way
    /// to see that it does.
    ///
    /// `schema()` is generated by `#[derive(Spec)]` and builds a fresh
    /// `Vec<DriverField>` — `name`, `doc` and a `ParamType` per field — on every
    /// call: 17 fields for `exec`, measured at 82 allocations and ~1.6 µs. This
    /// function runs for every target on every build, cache hits included, so
    /// asking the schema before knowing whether the config contains a `${` put
    /// that on the warm path of every workspace, including those using none of
    /// this. Nothing else observes the difference — the answers are identical
    /// either way — so without a counter the regression is invisible.
    #[tokio::test]
    async fn a_config_with_no_reference_never_asks_the_driver_for_its_schema() {
        use crate::engine::driver::{
            ApplyTransitiveResponse, ConfigRequest as DriverConfigRequest,
            ConfigResponse as DriverConfigResponse, Driver as RawDriver, ParseResponse, RunRequest,
            RunResponse,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting(Arc<AtomicUsize>);

        #[async_trait::async_trait]
        impl RawDriver for Counting {
            fn config(&self, _r: DriverConfigRequest) -> anyhow::Result<DriverConfigResponse> {
                Ok(DriverConfigResponse {
                    name: "counting".to_string(),
                })
            }
            fn schema(&self) -> crate::engine::driver::DriverSchema {
                self.0.fetch_add(1, Ordering::SeqCst);
                crate::engine::driver::DriverSchema {
                    accepts_deferred: true,
                    ..Default::default()
                }
            }
            async fn parse(
                &self,
                _r: crate::engine::driver::ParseRequest,
                _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
            ) -> anyhow::Result<ParseResponse> {
                anyhow::bail!("not used")
            }
            async fn apply_transitive(
                &self,
                _r: crate::engine::driver::ApplyTransitiveRequest,
                _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
            ) -> anyhow::Result<ApplyTransitiveResponse> {
                anyhow::bail!("not used")
            }
            async fn run<'a, 'io>(
                &self,
                _r: RunRequest<'a, 'io>,
                _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
            ) -> anyhow::Result<RunResponse> {
                anyhow::bail!("not used")
            }
            async fn run_shell<'a, 'io>(
                &self,
                _r: RunRequest<'a, 'io>,
                _c: &(dyn hcore::hasync::Cancellable + Send + Sync),
            ) -> anyhow::Result<RunResponse> {
                anyhow::bail!("not used")
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = crate::engine::Engine::new(crate::engine::Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            ..Default::default()
        })
        .expect("engine");
        engine
            .register_driver({
                let calls = Arc::clone(&calls);
                move |_init| Box::new(Counting(calls))
            })
            .expect("register");
        let engine = Arc::new(engine);

        let spec = |cfg: HashMap<String, Value>| crate::engine::provider::TargetSpec {
            addr: hmodel::htaddr::parse_addr("//auth:t").expect("addr"),
            driver: "counting".to_string(),
            config: cfg,
            ..Default::default()
        };

        // A config with no `${` anywhere: answered without touching the driver.
        engine
            .deferred_refs(
                &spec(cfg(&[("run", s("echo hi")), ("out", s("o.txt"))])),
                "counting",
            )
            .expect("no refs");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "schema built for nothing");

        // One that does carry a reference still consults it — the gate skips the
        // lookup, it does not remove it.
        engine
            .deferred_refs(&spec(cfg(&[("run", s("${read://infra:v}"))])), "counting")
            .expect("a reference");
        assert!(calls.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_plain_config_finds_nothing_and_allocates_nothing() {
        let refs =
            collect(&cfg(&[("run", s("echo hi")), ("out", s("o.txt"))]), &pkg()).expect("collect");
        assert!(refs.is_empty());
    }

    #[test]
    fn a_reference_is_found_with_the_path_that_wanted_it() {
        let refs =
            collect(&cfg(&[("role", s("${read://infra/aws:role-arn}"))]), &pkg()).expect("collect");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "role");
        assert_eq!(
            refs[0].producers[0].addr.to_string(),
            "//infra/aws:role-arn"
        );
    }

    /// The shape a flat per-field schema could not describe, and the reason the
    /// walk is over the raw value tree: a credential's `sources` is a list of maps
    /// of maps.
    #[test]
    fn a_reference_nested_inside_a_list_of_maps_is_found() {
        let inner = Value::Map(HashMap::from([(
            "present".to_string(),
            Value::Map(HashMap::from([(
                "env".to_string(),
                Value::Map(HashMap::from([(
                    "AWS_ROLE_ARN".to_string(),
                    s("${read://infra/aws:role-arn}"),
                )])),
            )])),
        )]));
        let refs =
            collect(&cfg(&[("sources", Value::List(vec![inner]))]), &pkg()).expect("collect");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "sources[0].present.env.AWS_ROLE_ARN");
    }

    #[test]
    fn a_relative_address_resolves_against_the_declaring_package() {
        let refs = collect(&cfg(&[("role", s("${read://auth:arn}"))]), &pkg()).expect("collect");
        assert_eq!(refs[0].producers[0].addr.to_string(), "//auth:arn");
    }

    #[test]
    fn a_template_may_mix_literals_and_references() {
        let refs = collect(
            &cfg(&[(
                "image",
                s("${read://infra:registry}/app:${read://infra:version}"),
            )]),
            &pkg(),
        )
        .expect("collect");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].producers.len(), 2);
        assert_eq!(inputs_for(&refs).len(), 2, "one edge per producer");
    }

    #[test]
    fn one_producer_named_twice_is_one_edge() {
        let refs = collect(
            &cfg(&[("a", s("${read://infra:x}")), ("b", s("${read://infra:x}"))]),
            &pkg(),
        )
        .expect("collect");
        assert_eq!(inputs_for(&refs).len(), 1);
    }

    /// The cell that matters. `runtime: true` would merge the producer's
    /// transitive tools, deps and env into every consumer and stage its file into
    /// the sandbox — a credential target must not inherit Terraform's environment.
    #[test]
    fn a_deferred_edge_is_hashed_and_not_staged() {
        let refs = collect(&cfg(&[("role", s("${read://infra:arn}"))]), &pkg()).expect("collect");
        let inputs = inputs_for(&refs);
        assert!(inputs[0].hashed, "the producer's content must reach hashin");
        assert!(
            !inputs[0].runtime,
            "the producer's environment must not reach the consumer"
        );
        assert!(inputs[0].origin_id.starts_with("option|"));
    }

    #[test]
    fn an_unrecognized_kind_is_not_ours_and_is_left_alone() {
        let refs = collect(
            &cfg(&[("run", s("echo ${FOO:-default} ${OUT} $$literal"))]),
            &pkg(),
        )
        .expect("collect");
        assert!(refs.is_empty());
    }

    /// The two kinds are two *cells*, and the dedup has to know that.
    ///
    /// `collect` sorts refs by their whole field text, so `"${read://a:b}"`
    /// sorts before `"${src://a:b}"`. Deduping on the address alone therefore
    /// kept the `read` edge and dropped the `src` one — the artifact was never
    /// staged, `${src://a:b}` resolved to a path that did not exist, and which
    /// of the two survived depended on the surrounding literal characters.
    #[test]
    fn one_producer_named_by_both_kinds_gets_both_edges() {
        let both = |a: &str, b: &str| {
            let refs = collect(&cfg(&[("x", s(a)), ("y", s(b))]), &pkg()).expect("collect");
            let mut inputs = inputs_for(&refs);
            inputs.sort_by_key(|i| i.runtime);
            assert_eq!(inputs.len(), 2, "{a} {b}");
            assert!(!inputs[0].runtime, "read is not staged");
            assert!(inputs[1].runtime, "src is staged");
            assert!(inputs.iter().all(|i| i.hashed));
            assert_ne!(inputs[0].origin_id, inputs[1].origin_id);
            assert!(
                inputs[1].annotations.contains_key(DEFERRED_SRC_ANNOTATION),
                "src is excluded from the transitive merge"
            );
            assert!(!inputs[0].annotations.contains_key(DEFERRED_SRC_ANNOTATION));
        };
        // Both sort orders, because the bug depended on which text sorted first.
        both("${read://a:b}", "${src://a:b}");
        both("z ${src://a:b}", "a ${read://a:b}");
    }

    /// A `${src:}` edge is `(hashed, staged)` and a `${read:}` edge is not.
    #[test]
    fn the_two_kinds_land_in_different_cells() {
        let src = inputs_for(&collect(&cfg(&[("v", s("${src://a:b}"))]), &pkg()).expect("collect"));
        assert_eq!(src.len(), 1);
        assert!(src[0].hashed && src[0].runtime);

        let read =
            inputs_for(&collect(&cfg(&[("v", s("${read://a:b}"))]), &pkg()).expect("collect"));
        assert_eq!(read.len(), 1);
        assert!(read[0].hashed && !read[0].runtime);
    }

    /// `|group` narrows a multi-output producer, exactly as it does on a dep.
    #[test]
    fn a_reference_may_name_an_output_group() {
        let refs = collect(&cfg(&[("v", s("${src://tools:cli|bin}"))]), &pkg()).expect("collect");
        assert_eq!(refs[0].producers[0].addr.output.as_deref(), Some("bin"));
        assert_eq!(refs[0].producers[0].addr.r#ref.format(), "//tools:cli");
    }

    /// Bash keeps every form it had: only an absolute address is heph's.
    #[test]
    fn a_shell_brace_form_is_not_collected() {
        for bash in [
            "${src:0:3}",
            "${src::3}",
            "${src::-1}",
            "${FOO:-default}",
            "${OUT}",
        ] {
            let refs = collect(&cfg(&[("run", s(bash))]), &pkg()).expect("collect");
            assert!(refs.is_empty(), "{bash}");
        }
    }

    /// An argument that *starts* like an address and then is not one is a typo,
    /// and is loud. One that never looked like an address at all is somebody
    /// else's syntax and is left alone — that is the whole claiming rule, and the
    /// two halves are tested together because the boundary is the point.
    #[test]
    fn a_malformed_address_is_rejected_but_a_shell_form_is_not() {
        let err = collect(&cfg(&[("role", s("${read://infra/role-arn}"))]), &pkg())
            .expect_err("a claimed argument that will not parse");
        let msg = format!("{err:#}");
        assert!(msg.contains("not a target address"), "{msg}");
        assert!(msg.contains("`role`"), "must name the field: {msg}");

        // Not claimed, so not heph's business — it reaches the tool verbatim,
        // exactly as it did before this feature existed.
        for untouched in ["${read:not an address}", "${read:infra:role-arn}"] {
            let refs = collect(&cfg(&[("role", s(untouched))]), &pkg()).expect("left alone");
            assert!(refs.is_empty(), "{untouched}");
        }
    }

    /// The order of synthesized inputs must not depend on `HashMap` iteration
    /// order, or a def's input list — and every diagnostic over it — reshuffles
    /// between runs.
    #[test]
    fn the_walk_is_deterministic() {
        let c = cfg(&[
            ("z", s("${read://infra:z}")),
            ("a", s("${read://infra:a}")),
            ("m", s("${read://infra:m}")),
        ]);
        let first = collect(&c, &pkg()).expect("collect");
        for _ in 0..8 {
            assert_eq!(collect(&c, &pkg()).expect("collect"), first);
        }
        let paths: Vec<&str> = first.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "m", "z"]);
    }
}
