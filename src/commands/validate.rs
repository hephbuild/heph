use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use enclose::enclose;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::error::{MultiError, TargetNotFoundError};
use crate::engine::{Engine, get_cwp, get_root, gitignore};
use crate::hmemoizer::downcast_chain_ref;
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
#[command(override_usage = "heph validate\n       heph validate <PACKAGE_MATCHER>")]
pub struct ValidateArgs {
    /// Package matcher (e.g. //pkg/...); omit to validate the whole workspace
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub matcher: Option<String>,
}

struct ValidateApp {
    engine: Arc<Engine>,
    /// Targets to validate — whole workspace or the user-scoped matcher.
    matcher: Matcher,
    /// True when the user passed a matcher; the gitignore check is then skipped.
    scoped: bool,
    root: PathBuf,
    fail_fast: bool,
}

#[async_trait]
impl App for ValidateApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new("Validating targets".to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new("Validating targets".to_string())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let ValidateApp {
            engine,
            matcher,
            scoped,
            root,
            fail_fast,
        } = self;
        let rs = engine.new_state_with_events(fail_fast, ctx.event_sender());

        // Overlap detection scopes to the user matcher when scoped, else uses the
        // codegen-tree selector (same one the gitignore enumeration uses).
        let overlap_matcher = if scoped {
            matcher.clone()
        } else {
            Matcher::TreeOutputTo(PkgBuf::from(""))
        };

        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            // 1. Link every in-scope target: parse + resolve its runtime inputs.
            //    No execution — proves the graph is well-formed.
            let link_fut = enclose!((engine, rs, matcher) async move {
                let addrs: Vec<Addr> = Arc::clone(&engine)
                    .query(rs.clone(), &matcher)
                    .try_collect()
                    .await?;
                let futs = addrs.iter().map(|addr| {
                    enclose!((engine, rs, addr) async move {
                        let def = match Arc::clone(&engine).get_def(rs.clone(), &addr).await {
                            Ok(def) => def,
                            // A provider may `list` an addr it cannot `get`
                            // standalone — e.g. go per-platform variants that only
                            // resolve as in-context deps. The query resolver skips
                            // these the same way. A NotFound for a *different* addr
                            // is a real missing dependency and still propagates
                            // (get_def surfaces it while expanding inputs).
                            Err(e)
                                if downcast_chain_ref::<TargetNotFoundError>(&e)
                                    .is_some_and(|nf| nf.addr == addr) =>
                            {
                                return Ok(());
                            }
                            Err(e) => return Err(e),
                        };
                        Arc::clone(&engine)
                            .link(rs.clone(), Arc::clone(&def.target_def))
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    })
                });
                // Always aggregate: validate reports every broken target, not
                // just the first one to fail.
                crate::engine::fanout::join_all_failable(futs, false).await?;
                Ok::<(), anyhow::Error>(())
            });

            // 2. Detect overlapping `codegen = copy` outputs.
            let overlap_fut = enclose!((engine, rs) async move {
                Arc::clone(&engine)
                    .codegen_copy_overlaps(rs.clone(), &overlap_matcher)
                    .await
            });

            // 3. Verify `.gitignore` is up to date (whole-workspace runs only).
            let gitignore_fut = enclose!((engine, rs) async move {
                if scoped {
                    return Ok::<bool, anyhow::Error>(false);
                }
                let entries = Arc::clone(&engine)
                    .codegen_copy_gitignore_patterns(
                        rs.clone(),
                        &Matcher::TreeOutputTo(PkgBuf::from("")),
                    )
                    .await?;
                let path = root.join(".gitignore");
                let existing = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
                };
                let want = gitignore::render(&existing, &entries);
                Ok(want != existing) // true = stale
            });

            // `join!` (not `try_join!`) so a failing check never short-circuits
            // the others — every check runs to completion and `finish` reports
            // all of their failures at once.
            let (link_res, overlap_res, gitignore_res) =
                tokio::join!(link_fut, overlap_fut, gitignore_fut);

            let overlap_res = overlap_res.map(|overlaps| {
                overlaps
                    .iter()
                    .map(|o| {
                        format!(
                            "codegen=copy outputs overlap: `{}` ({}) and `{}` ({})",
                            o.a.path,
                            o.a.addr.format(),
                            o.b.path,
                            o.b.addr.format()
                        )
                    })
                    .collect::<Vec<String>>()
            });
            let gitignore_res = gitignore_res.map(|stale| {
                if stale {
                    vec!["`.gitignore` is out of date — run `heph tool gen-gitignore`".to_string()]
                } else {
                    Vec::new()
                }
            });

            finish(vec![
                link_res.map(|()| Vec::new()),
                overlap_res,
                gitignore_res,
            ])?;

            // Success prints nothing; only the scoped-skip warning is emitted.
            if scoped {
                out.println("warning: skipped .gitignore freshness check (validation is scoped)");
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

/// Fold the outcome of every validate check into a single result. Each check
/// contributes either a list of human-readable problem strings (`Ok`) or a hard
/// error (`Err`); a check's `Err` that is itself a [`MultiError`] is flattened
/// so its inner errors surface individually. The point is exhaustiveness: nothing
/// short-circuits, so the user sees *all* the problems, not just the first.
fn finish(checks: Vec<anyhow::Result<Vec<String>>>) -> anyhow::Result<()> {
    let mut errs: Vec<anyhow::Error> = Vec::new();
    for check in checks {
        match check {
            Ok(problems) => errs.extend(problems.into_iter().map(|p| anyhow::anyhow!(p))),
            Err(e) => match e.downcast::<MultiError>() {
                Ok(MultiError(inner)) => errs.extend(inner),
                Err(e) => errs.push(e),
            },
        }
    }
    let combined = match errs.len() {
        0 => return Ok(()),
        1 => errs.pop().expect("len == 1"),
        _ => MultiError(errs).into(),
    };
    Err(combined).context("validation failed")
}

pub fn execute(args: &ValidateArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(
    args: ValidateArgs,
    sink: LogSink,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let root = get_root()?;
    let (matcher, scoped) = match args.matcher {
        Some(s) => (htpkg::parse(&s, &get_cwp()?)?, true),
        None => (Matcher::PackagePrefix(PkgBuf::from("")), false),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = ValidateApp {
        engine,
        matcher,
        scoped,
        root,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::finish;
    use crate::engine::error::MultiError;

    #[test]
    fn all_checks_ok_is_ok() {
        assert!(finish(vec![Ok(vec![]), Ok(vec![]), Ok(vec![])]).is_ok());
    }

    #[test]
    fn reports_every_problem_not_just_the_first() {
        // A link failure, an overlap, and a stale gitignore — all three must
        // appear in the rendered error, proving nothing short-circuited.
        let err = finish(vec![
            Err(anyhow::anyhow!("link broke for //pkg:a")),
            Ok(vec!["codegen overlap on `gen.rs`".to_string()]),
            Ok(vec![".gitignore is out of date".to_string()]),
        ])
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("link broke for //pkg:a"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("codegen overlap on `gen.rs`"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains(".gitignore is out of date"),
            "got: {rendered}"
        );
    }

    #[test]
    fn flattens_nested_multierror_from_a_check() {
        // The link fanout returns a MultiError when several targets fail; its
        // inner errors must be hoisted into the top-level list, not nested.
        let link_err = MultiError(vec![
            anyhow::anyhow!("target a failed"),
            anyhow::anyhow!("target b failed"),
        ]);
        let err = finish(vec![
            Err(link_err.into()),
            Ok(vec!["overlap c".to_string()]),
            Ok(vec![]),
        ])
        .unwrap_err();
        let multi = err
            .downcast_ref::<MultiError>()
            .expect("expected a flattened MultiError");
        assert_eq!(multi.0.len(), 3, "two link errors + one overlap");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("target a failed"), "got: {rendered}");
        assert!(rendered.contains("target b failed"), "got: {rendered}");
        assert!(rendered.contains("overlap c"), "got: {rendered}");
    }

    #[test]
    fn single_problem_is_returned_unwrapped() {
        // One problem stays a plain error (no "N errors:" envelope).
        let err = finish(vec![
            Ok(vec!["lonely overlap".to_string()]),
            Ok(vec![]),
            Ok(vec![]),
        ])
        .unwrap_err();
        assert!(err.downcast_ref::<MultiError>().is_none());
        assert!(format!("{err:#}").contains("lonely overlap"));
    }
}
