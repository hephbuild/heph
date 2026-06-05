use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use enclose::enclose;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::error::TargetNotFoundError;
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
                crate::engine::fanout::join_all_failable(futs, fail_fast).await?;
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
                    .codegen_copy_gitignore_patterns(rs.clone())
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

            let ((), overlaps, stale) = tokio::try_join!(link_fut, overlap_fut, gitignore_fut)?;

            let mut problems: Vec<String> = Vec::new();
            for o in &overlaps {
                let addrs = o
                    .addrs
                    .iter()
                    .map(|a| a.format())
                    .collect::<Vec<_>>()
                    .join(", ");
                problems.push(format!(
                    "codegen=copy output `{}` is produced by multiple targets: {}",
                    o.path, addrs
                ));
            }
            if stale {
                problems.push("`.gitignore` is out of date — run `heph gen-gitignore`".to_string());
            }
            if !problems.is_empty() {
                anyhow::bail!("validation failed:\n  {}", problems.join("\n  "));
            }

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
