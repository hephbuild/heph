//! BUILD-file language server.
//!
//! `heph tool build-lsp` runs this. heph owns the loop ([`server`]): it answers
//! `initialize`, keeps the open documents, and serves each request in-process.
//!
//! Every heph-specific answer derives from one classification of the cursor's
//! position, the [`site`] resolver's, computed from the document's tokens
//! ([`lex`]) and never from whether the file evaluated. The [`handlers`] table
//! maps each site kind to its completion items, hover and definition under one
//! merge policy. The standard Starlark surface (locals, symbol hover, goto-def
//! for local and loaded names, parse diagnostics) comes from the vendored
//! `starlark_lsp` analysis modules through [`context::HephLspContext`], which
//! passes through wherever heph cannot know the answer. `docs/LSP.md` describes
//! the site kinds, the merge policy and how to add one.

mod context;
pub(crate) mod diagnostics;
mod handlers;
mod index;
mod lex;
mod position;
mod server;
pub(crate) mod site;

#[cfg(feature = "test-support")]
pub use context::list_with_fresh_buildfile_provider;
#[cfg(feature = "test-support")]
pub use server::{RunOptions, run_with};

use handlers::{Dropped, ListingQuery, Refused};
use hcore::events::SelectionGap;
use hplugin::lsp::{Listed, ListedEntry, Listing, LspEngine, ScopeEndAck};
use hplugin::scope::ScopeId;
use lsp_server::Connection;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Serve the BUILD-file LSP over stdio until the client disconnects.
///
/// Must be called with a tokio runtime context entered: the LSP context
/// captures `Handle::current()` at construction (the engine's listings and the
/// per-buffer loader's walk run on it). `build_lsp.rs` enters the command
/// runtime before calling in.
pub fn serve_stdio(engine: Arc<dyn LspEngine>) -> anyhow::Result<()> {
    // `_io_threads` is intentionally never joined — see the `process::exit` note
    // below. Kept bound (not dropped) so the stdout writer thread stays alive to
    // flush responses until the process exits.
    let (stdio, _io_threads) = Connection::stdio();

    // Returns once the client sends `exit` (or stdin closes).
    run(stdio, engine)?;

    // Per the LSP spec, `exit` means terminate the process immediately. We must
    // NOT fall through to `io_threads.join()`: its stdin-reader thread blocks
    // until the client closes stdin, which editors (e.g. VS Code) only do
    // *after* the server has exited — so joining here would deadlock, the
    // client would SIGKILL us, and "restart server" would fail. The shutdown
    // response was already written and flushed (the client sequences `shutdown`
    // before `exit`), so there is nothing left to drain.
    std::process::exit(0);
}

/// Serve the BUILD-file LSP on `connection` until the client sends `exit` or
/// disconnects, then return.
///
/// The transport-free half of [`serve_stdio`]: the editor-shaped harness in
/// `tests/lsp.rs` drives it over an in-memory connection. Same runtime
/// contract as [`serve_stdio`].
pub fn run(connection: Connection, engine: Arc<dyn LspEngine>) -> anyhow::Result<()> {
    server::run(connection, engine)
}

/// How long `--explain` waits for the listing's scope to end before it
/// reports the scope still held. Past the engine's own bound on one
/// provider's `scope_ended` (5s), so a provider that hangs there shows as
/// `timed_out`, and "still held" means the scope itself never ended.
const SCOPE_END_WAIT: Duration = Duration::from_secs(10);

/// How many `dropped:` lines the text form prints; `--json` lists them all.
const DROPPED_SHOWN: usize = 20;

/// What the resolver makes of `<file>:<line>:<col>` (1-based line and column,
/// the column counted in characters): the site, why a heph answer would be
/// empty, and what a present one could not do in full. The same
/// classification every editor request goes through, for "why did it do
/// that?". `json` renders it as one JSON object.
///
/// At an address or load-path site it then runs the listing that site's
/// completion runs, on the engine `engine` builds (called only there, so any
/// other site needs no workspace), and adds the scope it ran in, each
/// candidate with the provider it came from, what the typed prefix dropped,
/// what the listing left out, and how each provider answered the end of the
/// scope. A listing that cannot run is a typed error in the output, not a
/// failed command. Must be called with a tokio runtime context entered, as
/// [`serve_stdio`] is.
pub fn explain(
    target: &str,
    json: bool,
    engine: impl FnOnce() -> anyhow::Result<Arc<dyn LspEngine>>,
) -> anyhow::Result<String> {
    explain_with(target, json, engine, SCOPE_END_WAIT)
}

/// [`explain`], waiting `scope_end_wait` for the scope to end.
fn explain_with(
    target: &str,
    json: bool,
    engine: impl FnOnce() -> anyhow::Result<Arc<dyn LspEngine>>,
    scope_end_wait: Duration,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let mut parts = target.rsplitn(3, ':');
    let (col, line, file) = match (parts.next(), parts.next(), parts.next()) {
        (Some(c), Some(l), Some(f)) => (c, l, f),
        _ => anyhow::bail!("expected <file>:<line>:<col>, got {target:?}"),
    };
    let line: u32 = line
        .parse()
        .with_context(|| format!("line {line:?} in {target:?}"))?;
    let col: u32 = col
        .parse()
        .with_context(|| format!("column {col:?} in {target:?}"))?;
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let tokens = lex::lex(&text);
    let parsed = starlark::syntax::AstModule::parse(
        file,
        text.clone(),
        &starlark::syntax::Dialect::Extended,
    )
    .ok();
    let overlay = parsed.as_ref().map(site::overlay_of);
    let pos = lsp_types::Position::new(line.saturating_sub(1), col.saturating_sub(1));
    let resolution = match position::to_offset(&text, pos, position::Encoding::Utf32) {
        Some(offset) => site::resolve(
            &site::Doc {
                text: &text,
                tokens: &tokens,
                overlay: overlay.as_ref(),
                ast: if overlay.is_some() {
                    site::AstState::Current
                } else {
                    site::AstState::Absent
                },
            },
            offset,
        ),
        None => site::Resolution::none(site::Reason::NotASite),
    };
    let value = resolution.to_json();
    let listing = explain_listing(file, &resolution.site, engine, scope_end_wait);
    if json {
        let mut out = serde_json::json!({ "file": file, "line": line, "col": col });
        if let (Some(map), serde_json::Value::Object(r)) = (out.as_object_mut(), value) {
            map.extend(r);
            if let Some(listing) = &listing {
                map.insert("listing".to_string(), listing.to_json());
            }
        }
        return serde_json::to_string_pretty(&out).context("rendering the resolution");
    }
    let none = serde_json::Value::Null;
    let site = value.get("site").unwrap_or(&none);
    let kind = site.get("kind").and_then(|k| k.as_str()).unwrap_or("none");
    let mut out = format!("{file}:{line}:{col}\n  site: {kind}");
    for key in ["call", "keyword", "prefix", "quote", "plugin", "selected"] {
        if let Some(v) = site.get(key).filter(|v| !v.is_null()) {
            out.push_str(&format!("\n  {key}: {v}"));
        }
    }
    if let Some(keys) = site
        .get("present_keys")
        .and_then(|k| k.as_array())
        .filter(|k| !k.is_empty())
    {
        let keys: Vec<&str> = keys.iter().filter_map(|k| k.as_str()).collect();
        out.push_str(&format!("\n  present keys: {}", keys.join(", ")));
    }
    match value.get("reason").filter(|r| !r.is_null()) {
        Some(r) => out.push_str(&format!("\n  reason: {r}")),
        None => out.push_str("\n  reason: none (heph answers here)"),
    }
    for d in value
        .get("degraded")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
    {
        out.push_str(&format!("\n  degraded: {d}"));
    }
    if let Some(listing) = &listing {
        listing.render(&mut out, scope_end_wait);
    }
    Ok(out)
}

/// Where an `--explain` listing stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailStage {
    /// The typed path would leave the workspace; no provider was asked.
    Path,
    /// The engine (the workspace) could not be built.
    EngineBuild,
    /// The engine's listing returned an error.
    List,
}

impl FailStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::EngineBuild => "engine_build",
            Self::List => "list",
        }
    }
}

/// The listing `--explain` ran at an address or load-path site.
enum ExplainedListing {
    /// No listing came back; `message` says what to do next.
    Failed {
        what: Option<Listing>,
        stage: FailStage,
        message: String,
        /// For a refused path, why ([`context::BadPath::as_str`]).
        reason: Option<&'static str>,
    },
    Ran(Box<Ran>),
}

/// A listing `--explain` ran, split the way the completion splits it.
struct Ran {
    query: ListingQuery,
    scope: ScopeId,
    /// How many entries the listing returned before the completion's filter.
    listed: usize,
    candidates: Vec<ListedEntry>,
    dropped: Vec<(ListedEntry, Dropped)>,
    skipped: Vec<SelectionGap>,
    /// How each provider answered the end of the scope; `None` when the
    /// scope was still held after the wait.
    acks: Option<Vec<ScopeEndAck>>,
    elapsed: Duration,
}

fn refused(r: Refused) -> ExplainedListing {
    ExplainedListing::Failed {
        what: None,
        stage: FailStage::Path,
        message: format!("//{} is not listed: {}", r.path, r.why.message()),
        reason: Some(r.why.as_str()),
    }
}

/// Runs the listing a completion at `site` runs, as `--explain` reports it;
/// `None` when the site lists nothing.
fn explain_listing(
    file: &str,
    site: &site::Site,
    engine: impl FnOnce() -> anyhow::Result<Arc<dyn LspEngine>>,
    scope_end_wait: Duration,
) -> Option<ExplainedListing> {
    // Whether the site lists, and whether a typed path is refused, is known
    // before the engine is built; only a `:name` needs it, for the open
    // document's package.
    if let Err(r) = handlers::listing_query(site, String::new)? {
        return Some(refused(r));
    }
    let engine = match engine() {
        Ok(engine) => engine,
        Err(e) => {
            return Some(ExplainedListing::Failed {
                what: None,
                stage: FailStage::EngineBuild,
                message: format!("the workspace failed to load: {e:#}"),
                reason: None,
            });
        }
    };
    // The same package rule as a completion's (`pkg_of`).
    let query = match handlers::listing_query(site, || {
        handlers::pkg_of(engine.root(), std::path::Path::new(file))
    })? {
        Ok(query) => query,
        Err(r) => return Some(refused(r)),
    };
    let started = Instant::now();
    let fut = Arc::clone(&engine).list(query.listing.clone());
    let listed = match futures::executor::block_on(fut) {
        Ok(listed) => listed,
        Err(e) => {
            return Some(ExplainedListing::Failed {
                what: Some(query.listing),
                stage: FailStage::List,
                message: format!("the listing failed: {e:#}"),
                reason: None,
            });
        }
    };
    let elapsed = started.elapsed();
    let Listed {
        scope,
        entries,
        skipped,
        scope_ended,
    } = listed;
    let acks = context::block_on_timeout(scope_ended, scope_end_wait);
    let listed = entries.len();
    let mut candidates = Vec::with_capacity(listed);
    let mut dropped = Vec::new();
    for e in entries {
        match query.drops(&e.name) {
            None => candidates.push(e),
            Some(why) => dropped.push((e, why)),
        }
    }
    Some(ExplainedListing::Ran(Box::new(Ran {
        query,
        scope,
        listed,
        candidates,
        dropped,
        skipped,
        acks,
        elapsed,
    })))
}

fn listing_json(what: &Listing) -> serde_json::Value {
    match what {
        Listing::Packages { under } => serde_json::json!({ "packages_under": under }),
        Listing::Targets { package } => serde_json::json!({ "targets_in": package }),
    }
}

fn listing_text(what: &Listing) -> String {
    match what {
        Listing::Packages { under } => format!("packages under //{under}"),
        Listing::Targets { package } => format!("targets in //{package}"),
    }
}

impl ExplainedListing {
    fn to_json(&self) -> serde_json::Value {
        match self {
            ExplainedListing::Failed {
                what,
                stage,
                message,
                reason,
            } => {
                let mut error = serde_json::json!({ "stage": stage.as_str(), "message": message });
                if let (Some(reason), Some(map)) = (reason, error.as_object_mut()) {
                    map.insert("reason".to_string(), (*reason).into());
                }
                serde_json::json!({
                    "query": what.as_ref().map(listing_json),
                    "error": error,
                })
            }
            ExplainedListing::Ran(ran) => serde_json::json!({
                "query": listing_json(&ran.query.listing),
                "prefix": ran.query.prefix,
                "scope": ran.scope.get(),
                "listed": ran.listed,
                "elapsed_ms": u64::try_from(ran.elapsed.as_millis()).unwrap_or(u64::MAX),
                "candidates": ran
                    .candidates
                    .iter()
                    .map(|e| serde_json::json!({ "name": e.name, "provider": &*e.provider }))
                    .collect::<Vec<_>>(),
                "dropped": ran
                    .dropped
                    .iter()
                    .map(|(e, why)| serde_json::json!({
                        "name": e.name,
                        "provider": &*e.provider,
                        "reason": why.as_str(),
                    }))
                    .collect::<Vec<_>>(),
                "skipped": ran.skipped,
                "scope_ended": ran.acks.as_ref().map(|acks| acks
                    .iter()
                    .map(|a| {
                        serde_json::json!({ "provider": a.provider, "outcome": a.outcome.as_str() })
                    })
                    .collect::<Vec<_>>()),
            }),
        }
    }

    fn render(&self, out: &mut String, scope_end_wait: Duration) {
        match self {
            ExplainedListing::Failed {
                what,
                stage,
                message,
                ..
            } => {
                if let Some(what) = what {
                    out.push_str(&format!("\n  listing: {}", listing_text(what)));
                }
                out.push_str(&format!("\n  error ({}): {message}", stage.as_str()));
            }
            ExplainedListing::Ran(ran) => ran.render(out, scope_end_wait),
        }
    }
}

impl Ran {
    fn render(&self, out: &mut String, scope_end_wait: Duration) {
        let prefix = &self.query.prefix;
        out.push_str(&format!(
            "\n  listing: {}, kept if starting with {prefix:?}\n  scope: {}\n  listed: {}, candidates: {} (in {:.0?})",
            listing_text(&self.query.listing),
            self.scope,
            self.listed,
            self.candidates.len(),
            self.elapsed,
        ));
        for e in &self.candidates {
            out.push_str(&format!("\n  candidate: {:?} from {}", e.name, e.provider));
        }
        for (e, why) in self.dropped.iter().take(DROPPED_SHOWN) {
            let because = match why {
                Dropped::Prefix => format!("does not start with {prefix:?}"),
                Dropped::RootPackage => "the root package is not a load path".to_string(),
            };
            out.push_str(&format!(
                "\n  dropped: {:?} from {} ({because})",
                e.name, e.provider
            ));
        }
        let more = self.dropped.len().saturating_sub(DROPPED_SHOWN);
        if more > 0 {
            out.push_str(&format!(
                "\n  dropped: …and {more} more (--json lists them)"
            ));
        }
        for g in &self.skipped {
            for e in &g.examples {
                out.push_str(&format!(
                    "\n  skipped: {} {} {}: {}",
                    g.stage, g.provider, e.scope, e.cause
                ));
            }
            let more = g.count.saturating_sub(g.examples.len());
            if more > 0 {
                out.push_str(&format!(
                    "\n  skipped: {} {} …and {more} more",
                    g.stage, g.provider
                ));
            }
        }
        if self.listed == 0 {
            match &self.query.listing {
                Listing::Targets { package } => {
                    out.push_str(&format!("\n  no provider listed //{package}"));
                }
                Listing::Packages { under } => {
                    out.push_str(&format!("\n  no provider listed a package under //{under}"));
                }
            }
        }
        if self.candidates.is_empty() && !self.skipped.is_empty() {
            out.push_str(
                "\n  no candidates: the skipped: lines above are what the listing left out",
            );
        }
        if self.elapsed > context::LISTING_WAIT {
            out.push_str(&format!(
                "\n  note: the listing took {:.1?}; a completion waits {:?}, then answers stock (reason listing_timed_out)",
                self.elapsed,
                context::LISTING_WAIT,
            ));
        }
        match &self.acks {
            None => out.push_str(&format!(
                "\n  scope {} still held after {scope_end_wait:?}",
                self.scope
            )),
            Some(acks) if acks.is_empty() => out.push_str("\n  scope ended: no provider was told"),
            Some(acks) => {
                for a in acks {
                    out.push_str(&format!("\n  scope ended: {} {}", a.provider, a.outcome));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::context::tests::{FakeEngine, ListMode};
    use super::explain_with;
    use hplugin::lsp::LspEngine;
    use std::sync::Arc;
    use std::time::Duration;

    /// A workspace with `pkg/BUILD` holding `text`, and `--explain` at the end
    /// of `needle` in it, through an engine listing as `list` says.
    fn explain_at(
        files: &[(&str, &str)],
        file: &str,
        needle: &str,
        json: bool,
        engine: Option<ListMode>,
    ) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, body) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, body).expect("write");
        }
        let text = files
            .iter()
            .find(|(p, _)| *p == file)
            .map(|(_, t)| *t)
            .expect("the explained file is one of the files");
        let offset = text.find(needle).expect("needle") + needle.len();
        let before = text.get(..offset).expect("char boundary");
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        let target = format!("{}:{line}:{col}", dir.path().join(file).display());
        let root = dir.path().to_path_buf();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let _rt = rt.enter();
        explain_with(
            &target,
            json,
            || match engine {
                Some(list) => Ok(Arc::new(FakeEngine { root, list }) as Arc<dyn LspEngine>),
                None => anyhow::bail!("config: no such plugin \"nope\""),
            },
            Duration::from_millis(200),
        )
        .expect("explain answers")
    }

    fn json(out: &str) -> serde_json::Value {
        serde_json::from_str(out).expect("json")
    }

    /// The workspace failing to load is a typed `engine_build` error, and
    /// the command still answers.
    #[test]
    fn an_engine_that_fails_to_build_is_an_engine_build_error() {
        let files = [("app/BUILD", "x = [\"//ap\"]\n")];
        let v = json(&explain_at(&files, "app/BUILD", "//ap", true, None));
        assert_eq!(v["listing"]["error"]["stage"], "engine_build", "{v}");
        let msg = v["listing"]["error"]["message"].as_str().unwrap_or("");
        assert!(msg.starts_with("the workspace failed to load: "), "{msg}");
        assert!(msg.contains("no such plugin"), "{msg}");

        let out = explain_at(&files, "app/BUILD", "//ap", false, None);
        assert!(
            out.contains(
                "error (engine_build): the workspace failed to load: config: no such plugin"
            ),
            "{out}"
        );
    }

    /// The engine's listing failing is a typed `list` error, naming the query.
    #[test]
    fn a_listing_that_errs_is_a_list_error() {
        let files = [("app/BUILD", "x = [\"//app:\"]\n")];
        let v = json(&explain_at(
            &files,
            "app/BUILD",
            "//app:",
            true,
            Some(ListMode::Fails),
        ));
        assert_eq!(v["listing"]["query"]["targets_in"], "app", "{v}");
        assert_eq!(v["listing"]["error"]["stage"], "list", "{v}");
        let msg = v["listing"]["error"]["message"].as_str().unwrap_or("");
        assert!(msg.contains("the provider exploded"), "{msg}");

        let out = explain_at(&files, "app/BUILD", "//app:", false, Some(ListMode::Fails));
        assert!(out.contains("listing: targets in //app"), "{out}");
        assert!(
            out.contains("error (list): the listing failed: the provider exploded"),
            "{out}"
        );
    }

    /// A traversal is refused before any engine is built: `path` stage, the
    /// reason, and no provider asked.
    #[test]
    fn a_traversal_prefix_is_refused_before_the_engine() {
        for (needle, reason) in [
            ("//../..", "dot_segment"),
            ("///etc", "absolute"),
            ("//a//b", "empty_segment"),
        ] {
            let text = format!("x = [\"{needle}\"]\n");
            let files = [("app/BUILD", text.as_str())];
            // `None`: building the engine would fail the explain differently.
            let v = json(&explain_at(&files, "app/BUILD", needle, true, None));
            assert_eq!(v["listing"]["error"]["stage"], "path", "{needle}: {v}");
            assert_eq!(v["listing"]["error"]["reason"], reason, "{needle}: {v}");
            assert!(v["listing"].get("candidates").is_none(), "{v}");
        }
        let files = [("app/BUILD", "load(\"//../..\", \"x\")\n")];
        let out = explain_at(&files, "app/BUILD", "//../..", false, None);
        assert!(
            out.contains("error (path): //../.. is not listed: a `.` or `..` segment"),
            "{out}"
        );
    }

    /// A `load("//` path site lists packages and drops the root package,
    /// which a load path cannot name; what the prefix dropped is shown.
    #[test]
    fn a_load_path_site_drops_the_root_package_and_says_so() {
        let files = [
            ("BUILD", ""),
            ("lib/BUILD", ""),
            ("other/BUILD", ""),
            ("app/BUILD", "load(\"//l\", \"x\")\n"),
        ];
        let v = json(&explain_at(
            &files,
            "app/BUILD",
            "//l",
            true,
            Some(ListMode::Fresh),
        ));
        let listing = &v["listing"];
        assert_eq!(listing["query"]["packages_under"], "", "{v}");
        assert_eq!(
            listing["candidates"],
            serde_json::json!([{ "name": "lib", "provider": "buildfile" }]),
            "{v}"
        );
        let dropped = listing["dropped"].as_array().cloned().unwrap_or_default();
        assert!(
            dropped.contains(&serde_json::json!({ "name": "", "provider": "buildfile", "reason": "root_package" })),
            "{v}"
        );
        assert!(
            dropped.contains(
                &serde_json::json!({ "name": "other", "provider": "buildfile", "reason": "prefix" })
            ),
            "{v}"
        );

        let out = explain_at(&files, "app/BUILD", "//l", false, Some(ListMode::Fresh));
        assert!(
            out.contains("dropped: \"\" from buildfile (the root package is not a load path)"),
            "{out}"
        );
        assert!(
            out.contains("dropped: \"other\" from buildfile (does not start with \"l\")"),
            "{out}"
        );
    }

    /// `:name` in the root package's BUILD lists the root package's targets
    /// (`Targets { package: "" }`), and an empty listing says no provider
    /// listed it.
    #[test]
    fn a_relative_name_in_the_root_package_lists_the_root() {
        let files = [(
            "BUILD",
            "target(name = \"top\", driver = \"exec\")\nx = [\":t\"]\n",
        )];
        let v = json(&explain_at(
            &files,
            "BUILD",
            "\":t",
            true,
            Some(ListMode::Fresh),
        ));
        assert_eq!(v["listing"]["query"]["targets_in"], "", "{v}");
        assert_eq!(
            v["listing"]["candidates"],
            serde_json::json!([{ "name": "top", "provider": "buildfile" }]),
            "{v}"
        );

        let files = [("app/BUILD", "x = [\"//nope:\"]\n")];
        let out = explain_at(&files, "app/BUILD", "//nope:", false, Some(ListMode::Fresh));
        assert!(out.contains("no provider listed //nope"), "{out}");
    }

    /// A scope that never ends is reported held, after a bounded wait.
    #[test]
    fn a_scope_that_never_ends_is_reported_held() {
        let files = [("app/BUILD", "x = [\"//app:\"]\n")];
        let v = json(&explain_at(
            &files,
            "app/BUILD",
            "//app:",
            true,
            Some(ListMode::ScopeHeld),
        ));
        assert!(v["listing"]["scope_ended"].is_null(), "{v}");
        let out = explain_at(
            &files,
            "app/BUILD",
            "//app:",
            false,
            Some(ListMode::ScopeHeld),
        );
        assert!(out.contains(" still held after 200ms"), "{out}");
    }
}
