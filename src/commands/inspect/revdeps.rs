use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use enclose::enclose;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::Engine;
use crate::engine::error::{CycleError, TargetNotFoundError};
use crate::engine::request_state::RequestState;
use crate::hmemoizer::downcast_chain_ref;
use crate::htaddr::{self, Addr};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target whose users to find: a `//pkg:name` address, or a `./path` /
    /// `../path` file (sugar for its `fs` file target)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Restrict the search to packages matching this matcher (e.g. //pkg/...);
    /// defaults to the whole workspace
    #[arg(long, value_name = "PACKAGE_MATCHER")]
    pub scope: Option<String>,
}

struct RevdepsApp {
    engine: Arc<Engine>,
    /// The target whose reverse dependencies (users) we want.
    addr: Addr,
    /// Packages to search for users (whole workspace by default).
    scope: Matcher,
    fail_fast: bool,
}

#[async_trait]
impl App for RevdepsApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Revdeps {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Revdeps {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let RevdepsApp {
            engine,
            addr,
            scope,
            fail_fast,
        } = self;
        let rs = engine.new_state_with_events(fail_fast, ctx.event_sender());

        // Resolving in-scope targets records rich failures in `rs`, which
        // `finalize` prefers over the returned error.
        let res = revdeps(Arc::clone(&engine), rs.clone(), &addr, &scope, fail_fast).await;
        crate::commands::errors::finalize!(ctx, rs, res, users => {
            for user in &users {
                println!("{}", user.format());
            }
            Ok(())
        })
    }
}

/// Find every target in `scope` that declares `target` as a direct dependency —
/// the reverse of `inspect deps`. Direct (not transitive) edges only: this
/// answers "which targets reference this one", the precise inverse of the build
/// graph edge. Results are sorted for deterministic output.
async fn revdeps(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    target: &Addr,
    scope: &Matcher,
    fail_fast: bool,
) -> anyhow::Result<Vec<Addr>> {
    let candidates: Vec<Addr> = Arc::clone(&engine)
        .query(rs.clone(), scope)
        .try_collect()
        .await
        .context("listing candidate targets")?;

    let futs = candidates.iter().map(|candidate| {
        enclose!((engine, rs, candidate, target) async move {
            let def = match Arc::clone(&engine).get_direct_def(rs.clone(), &candidate).await {
                Ok(def) => def,
                // A provider may `list` an addr it cannot `get` standalone (e.g.
                // per-platform variants that only resolve as in-context deps), or
                // a candidate may cycle back to the query target. Neither can be a
                // resolvable user; skip it the way the query resolver does. A
                // NotFound for a *different* addr is a real breakage and still
                // propagates.
                Err(e)
                    if downcast_chain_ref::<TargetNotFoundError>(&e)
                        .is_some_and(|nf| nf.addr == candidate) =>
                {
                    return Ok(None);
                }
                Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => return Ok(None),
                Err(e) => return Err(e),
            };
            let uses = def
                .target_def
                .inputs
                .iter()
                .any(|input| input.r#ref.r#ref == target);
            Ok::<Option<Addr>, anyhow::Error>(uses.then(|| candidate.clone()))
        })
    });

    let found = crate::engine::fanout::join_all_failable(futs, fail_fast).await?;
    let mut users: Vec<Addr> = found.into_iter().flatten().collect();
    users.sort_by_key(|a| a.format());
    Ok(users)
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

/// Resolve a CLI target argument into an `Addr`. A `./` or `../` prefix is sugar
/// for an `fs` file target: the path is resolved against the current package and
/// rewritten to `//@heph/fs:file@f=<root-relative path>` so it matches the addr
/// other targets reference the file by. Anything else is a plain target address.
fn resolve_addr_in(input: &str, cwp: &PkgBuf) -> anyhow::Result<Addr> {
    if input.starts_with("./") || input.starts_with("../") {
        let rel = htpkg::join_rel_checked(cwp.as_str(), input)
            .with_context(|| format!("resolving file path {input}"))?;
        Ok(crate::pluginfs::file_addr(&rel))
    } else {
        htaddr::parse_addr(input).with_context(|| format!("parse {input}"))
    }
}

/// `resolve_addr_in` against the current working package.
fn resolve_addr(input: &str) -> anyhow::Result<Addr> {
    resolve_addr_in(input, &crate::engine::get_cwp()?)
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let addr = resolve_addr(args.addr.as_ref())?;
    let scope = match &args.scope {
        Some(s) => htpkg::parse(s.as_str(), &crate::engine::get_cwp()?)
            .with_context(|| format!("parse scope {s}"))?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RevdepsApp {
        engine,
        addr,
        scope,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::{resolve_addr_in, revdeps};
    use crate::engine::{Config, Engine};
    use crate::htaddr::parse_addr;
    use crate::htmatcher::Matcher;
    use crate::htpkg::PkgBuf;
    use crate::pluginstatictarget;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn dot_slash_path_resolves_to_fs_file_addr() {
        // `./somefile.txt` in package `cmd/server` → the fs file target keyed by
        // the root-relative path.
        let addr = resolve_addr_in("./somefile.txt", &PkgBuf::from("cmd/server")).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/somefile.txt"));
    }

    #[test]
    fn dot_dot_path_normalizes_against_package() {
        let addr = resolve_addr_in("../shared/x.txt", &PkgBuf::from("cmd/server")).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/shared/x.txt"));
    }

    #[test]
    fn path_at_root_package() {
        let addr = resolve_addr_in("./a.txt", &PkgBuf::from("")).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("a.txt"));
    }

    #[test]
    fn plain_addr_is_parsed_verbatim() {
        let addr = resolve_addr_in("//lib:core", &PkgBuf::from("cmd/server")).unwrap();
        assert_eq!(addr, parse_addr("//lib:core").unwrap());
    }

    /// A static `exec` target depending on `deps` (default group) — addr strings.
    fn target(addr: &str, deps: &[&str]) -> pluginstatictarget::Target {
        let mut dep_map = HashMap::new();
        if !deps.is_empty() {
            dep_map.insert(
                String::new(),
                deps.iter().map(|d| (*d).to_string()).collect(),
            );
        }
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            deps: dep_map,
            ..Default::default()
        }
    }

    fn make_engine(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_managed_driver(|_| Box::new(crate::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    fn all() -> Matcher {
        Matcher::PackagePrefix(PkgBuf::from(""))
    }

    #[tokio::test]
    async fn finds_direct_users() -> anyhow::Result<()> {
        // //lib:core is used by //app:a and //app:b, not //other:c.
        let (engine, _root) = make_engine(vec![
            target("//lib:core", &[]),
            target("//app:a", &["//lib:core"]),
            target("//app:b", &["//lib:core"]),
            target("//other:c", &[]),
        ])?;
        let rs = engine.new_state();
        let core = parse_addr("//lib:core")?;

        let users = revdeps(Arc::clone(&engine), rs, &core, &all(), false).await?;

        let users: Vec<String> = users.iter().map(|a| a.format()).collect();
        assert_eq!(users, vec!["//app:a".to_string(), "//app:b".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn scope_restricts_search() -> anyhow::Result<()> {
        let (engine, _root) = make_engine(vec![
            target("//lib:core", &[]),
            target("//app:a", &["//lib:core"]),
            target("//tool:t", &["//lib:core"]),
        ])?;
        let rs = engine.new_state();
        let core = parse_addr("//lib:core")?;

        // Only search within //app — //tool:t must not appear.
        let scope = Matcher::PackagePrefix(PkgBuf::from("app"));
        let users = revdeps(Arc::clone(&engine), rs, &core, &scope, false).await?;

        let users: Vec<String> = users.iter().map(|a| a.format()).collect();
        assert_eq!(users, vec!["//app:a".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn no_users_is_empty() -> anyhow::Result<()> {
        let (engine, _root) = make_engine(vec![target("//lib:core", &[]), target("//app:a", &[])])?;
        let rs = engine.new_state();
        let core = parse_addr("//lib:core")?;

        let users = revdeps(Arc::clone(&engine), rs, &core, &all(), false).await?;

        assert!(users.is_empty());
        Ok(())
    }
}
