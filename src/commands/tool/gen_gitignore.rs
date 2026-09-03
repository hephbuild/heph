use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::{Engine, get_cwp, get_root, gitignore};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
#[command(
    override_usage = "heph tool gen-gitignore\n       heph tool gen-gitignore <PACKAGE_MATCHER>"
)]
pub struct Args {
    /// Package matcher (e.g. //pkg/...); omit to regenerate the whole section.
    /// When given, only the lines emitted by targets under that matcher are
    /// rebuilt — a smaller graph walk, so a faster run.
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub matcher: Option<String>,
}

struct GenGitignoreApp {
    engine: Arc<Engine>,
    /// Targets to scan for codegen-copy outputs. Whole-workspace selector, or
    /// the user's package matcher when scoped.
    matcher: Matcher,
    /// True when the user passed a matcher: rebuild only the matching slice of
    /// the section and preserve every other line.
    scoped: bool,
    root: PathBuf,
    fail_fast: bool,
}

#[async_trait]
impl App for GenGitignoreApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new("Generating .gitignore".to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new("Generating .gitignore".to_string())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());

        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            // One whole-or-scoped resolution feeds both derived artifacts: the
            // claim store (which decides what is generated) and the `.gitignore`
            // section (which only tells git to ignore build outputs). They ride
            // the same command because the resolution is the expensive part, not
            // because they are the same thing.
            let scan = Arc::clone(&self.engine)
                .codegen_copy_scan_for(rs.clone(), &self.matcher)
                .await?;
            let fresh = gitignore::normalize_entries(scan.gitignore_entries());

            let path = self.root.join(".gitignore");
            let existing = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", path.display()));
                }
            };
            // Scoped: graft the freshly-scanned slice over the existing section,
            // keeping every line emitted by a target outside the matcher.
            // Whole-workspace: replace the section wholesale.
            // Reconcile the codegen claim ledger from the same freshly-resolved
            // data, before touching `.gitignore`. The ledger is what decides which
            // tree paths are generated; the `.gitignore` section below only tells
            // git to ignore build outputs. They ride the same command because both
            // need one whole-workspace resolution.
            //
            // Reconciliation is needed because the write-back that maintains the
            // ledger only ever sees targets that still exist and still run, so a
            // target deleted from the tree keeps its old claim indefinitely — and
            // a stale claim silently hides a real source file at that path.
            let dropped = gitignore::reconcile_claims(
                self.engine.codegen_claims(),
                &scan,
                &self.matcher,
                self.scoped,
            )
            .context("reconciling the codegen claim ledger")?;
            for addr in &dropped {
                out.println(format!(
                    "Released codegen claims for {addr} (no longer emits codegen = \"copy\" output)"
                ));
            }

            let entries = if self.scoped {
                gitignore::merge_section(&existing, fresh, &self.matcher)
            } else {
                fresh
            };
            let updated = gitignore::render(&existing, &entries);
            if updated == existing {
                out.println(format!("{} already up to date", path.display()));
            } else {
                std::fs::write(&path, &updated)
                    .with_context(|| format!("writing {}", path.display()))?;
                out.println(format!(
                    "Updated {} ({} entr{})",
                    path.display(),
                    entries.len(),
                    if entries.len() == 1 { "y" } else { "ies" }
                ));
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let root = get_root()?;
    // Scoped runs walk only the matched packages; whole-workspace runs use the
    // codegen-tree selector that reaches every codegen target.
    let (matcher, scoped) = match args.matcher {
        Some(s) => (htpkg::parse(&s, &get_cwp()?)?, true),
        None => (Matcher::TreeOutputTo(PkgBuf::from("")), false),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = GenGitignoreApp {
        engine,
        matcher,
        scoped,
        root,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
