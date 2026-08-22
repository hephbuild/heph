use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use enclose::enclose;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::error::MultiError;
use crate::engine::query::skip_unresolvable;
use crate::engine::{Engine, get_cwp, get_root, gitignore};
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
            // Each check runs to completion before the next begins, and that is
            // a correctness requirement rather than a style choice.
            //
            // Each drives its own `Engine::query` walk, and a walk's
            // `MatchShrug` arm resolves candidates on a *speculative*
            // `RequestState` whose cycle check walks per-chain breadcrumbs
            // instead of the shared `DepDag`. That is sound only while one such
            // chain exists at a time — a guarantee `Engine::query` makes *per
            // walk* (its arm sits in the consumer of its own fan-out) and
            // therefore cannot make across walks. `Engine::query`'s own comment
            // names this command as the way the guarantee gets broken.
            //
            // Two chains at once are mutually invisible, and both consequences
            // reach a build definition: `mem_spec`/`mem_def` are shared and keyed
            // by addr, so whichever chain creates the cell decides which provider
            // resolved the addr — and `hashin` folds `def.driver`, so the race
            // picks the cache key. And two chains that resolve each other close a
            // cycle neither can see, which hangs where the serial path reported an
            // error. Both are reachable here: unscoped, checks 2 and 3 run the
            // *same* `TreeOutputTo("")` matcher, which shrugs at the addr *and*
            // the spec level, so both walks drive `get_spec` and `get_def`
            // speculatively over the whole workspace.
            //
            // Serialising costs much less than three times one walk: the three
            // share this request's memoizers, so the second and third walk hit
            // cells the first already filled. It is the *speculative chains* that
            // must not overlap, not the work they memoize.
            //
            // Every result is bound rather than `?`-propagated, so a failing
            // check never short-circuits the others and `finish` reports all of
            // them at once.

            // 1. Link every in-scope target: parse + resolve its runtime inputs.
            //    No execution — proves the graph is well-formed.
            let link_res: anyhow::Result<()> = async {
                let addrs: Vec<Addr> = Arc::clone(&engine)
                    .query(rs.clone(), &matcher)
                    .try_collect()
                    .await?;
                let futs = addrs.iter().map(|addr| {
                    enclose!((engine, rs, addr) async move {
                        // A listed candidate that doesn't resolve standalone —
                        // e.g. go per-platform variants that only resolve as
                        // in-context deps — has no graph of its own to link. The
                        // helper rather than a `query_*` stream: the fan-out below
                        // reports *every* failure, and a stream stops at the
                        // first.
                        let res = Arc::clone(&engine).get_def(rs.clone(), &addr).await;
                        let Some(def) = skip_unresolvable(&addr, res)? else {
                            return Ok(());
                        };
                        Arc::clone(&engine)
                            .link(rs.clone(), Arc::clone(&def.target_def))
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    })
                });
                // Always aggregate: validate reports every broken target, not
                // just the first one to fail. This fan-out is genuinely
                // concurrent and stays so — it resolves real deps on `rs`, not
                // speculative candidates, so it starts no chain.
                crate::engine::fanout::join_all_failable(futs, false).await?;
                Ok(())
            }
            .await;

            // 2. Detect overlapping `codegen = copy` outputs.
            let overlap_res = Arc::clone(&engine)
                .codegen_copy_overlaps(rs.clone(), &overlap_matcher)
                .await;

            // 3. Verify `.gitignore` and the codegen claim ledger are up to date
            //    (whole-workspace runs only). Both are derived from the same
            //    freshly-resolved set of `codegen = "copy"` outputs.
            let gitignore_res: anyhow::Result<(bool, Vec<String>)> = async {
                if scoped {
                    return Ok((false, Vec::new()));
                }
                let entries = Arc::clone(&engine)
                    .codegen_copy_gitignore_patterns(
                        rs.clone(),
                        &Matcher::TreeOutputTo(PkgBuf::from("")),
                    )
                    .await?;

                // Claims whose target no longer emits them. Reported, not
                // repaired: `validate` is a check, and the repair belongs to the
                // command that rewrites these declarations. A stale claim is the
                // quiet failure — it hides a real source file at that path — so
                // silence here would be the wrong kind of clean run.
                let want = gitignore::entries_by_addr(&entries);
                let orphans: Vec<String> = engine
                    .codegen_claims()
                    .entries()
                    .context("reading the codegen claim ledger")?
                    .into_keys()
                    .filter(|addr| !want.contains_key(addr))
                    .collect();

                let path = root.join(".gitignore");
                let existing = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
                };
                let rendered = gitignore::render(&existing, &entries);
                Ok((rendered != existing, orphans))
            }
            .await;

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
            let gitignore_res = gitignore_res.map(|(stale, orphans)| {
                let mut msgs = Vec::new();
                if stale {
                    msgs.push(
                        "`.gitignore` is out of date — run `heph tool gen-gitignore`".to_string(),
                    );
                }
                for addr in orphans {
                    msgs.push(format!(
                        "codegen claim for `{addr}` outlives the target — it no longer emits \
                         codegen = \"copy\" output, so its claimed paths are hidden from every \
                         glob; run `heph tool gen-gitignore`"
                    ));
                }
                msgs
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
