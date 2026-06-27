use std::collections::BTreeSet;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use hplugin::lsp::LspEngine;
use hxstarlark_fmt::{Config, FmtError};

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::{Engine, get_cwp, get_root};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::pluginbuildfile::{
    build_file_patterns_from_options, build_files_in_dir, default_build_file_patterns,
};
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

/// The argument value that selects stdin → stdout mode.
const STDIN_MARKER: &str = "-";

#[derive(clap::Args, Clone)]
#[command(
    override_usage = "heph tool build-fmt\n       heph tool build-fmt <PACKAGE_MATCHER>\n       heph tool build-fmt -"
)]
pub struct Args {
    /// Package matcher (e.g. //pkg/...); omit to format every package's BUILD
    /// file. Pass `-` to read source from stdin and write the formatted result
    /// to stdout.
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub matcher: Option<String>,
    /// Check mode: exit non-zero if any file is not formatted; do not write.
    #[arg(long)]
    pub check: bool,
    /// Number of spaces per indentation level.
    #[arg(long, default_value_t = 2)]
    pub indent: usize,
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    if args.matcher.as_deref() == Some(STDIN_MARKER) {
        return format_stdin(args.indent);
    }
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

/// Read Starlark from stdin, write the formatted result to stdout.
fn format_stdin(indent: usize) -> anyhow::Result<()> {
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .context("reading stdin")?;

    let out = match hxstarlark_fmt::format(
        "<stdin>",
        &src,
        Config {
            indent_size: indent,
        },
    ) {
        Ok(out) => out,
        // A skipped file is emitted unchanged, matching in-place behavior.
        Err(FmtError::Skip) => src,
        Err(e) => return Err(anyhow::anyhow!(e).context("formatting stdin")),
    };

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(out.as_bytes()).context("writing stdout")?;
    lock.flush().context("flushing stdout")?;
    Ok(())
}

struct BuildFmtApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    root: PathBuf,
    check: bool,
    indent: usize,
    fail_fast: bool,
}

#[async_trait]
impl App for BuildFmtApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new("Formatting BUILD files".to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new("Formatting BUILD files".to_string())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());

        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            let packages: Vec<String> = Arc::clone(&self.engine)
                .packages(&self.matcher, &rs)
                .await?
                .collect::<anyhow::Result<Vec<_>>>()?;

            // Resolve the workspace's configured BUILD-file patterns (literal
            // names like `BUILD` and globs like `*.BUILD`) so every file the
            // buildfile provider treats as a BUILD file is covered.
            let opts = LspEngine::provider_options(self.engine.as_ref(), "buildfile");
            let patterns = build_file_patterns_from_options(&opts)
                .unwrap_or_else(|_| default_build_file_patterns());

            // A package may have several matching files; dedup across packages.
            let mut build_files: BTreeSet<PathBuf> = BTreeSet::new();
            for pkg in &packages {
                let dir = self.root.join(pkg);
                build_files.extend(build_files_in_dir(&dir, &patterns));
            }

            let cfg = Config {
                indent_size: self.indent,
            };
            let mut changed: Vec<String> = Vec::new();
            let mut formatted = 0usize;

            for path in &build_files {
                let label = file_label(&self.root, path);
                let src = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;

                let out_text = match hxstarlark_fmt::format(&path.to_string_lossy(), &src, cfg) {
                    Ok(text) => text,
                    Err(FmtError::Skip) => continue,
                    Err(e) => {
                        return Err(
                            anyhow::anyhow!(e).context(format!("formatting {}", path.display()))
                        );
                    }
                };

                if out_text == src {
                    continue;
                }

                if self.check {
                    changed.push(label);
                } else {
                    std::fs::write(path, &out_text)
                        .with_context(|| format!("writing {}", path.display()))?;
                    out.println(format!("Formatted {label}"));
                    formatted += 1;
                }
            }

            if self.check {
                if changed.is_empty() {
                    out.println("All BUILD files are formatted");
                } else {
                    for label in &changed {
                        out.println(format!("Would reformat {label}"));
                    }
                    return Err(anyhow::anyhow!(
                        "{} BUILD file(s) need formatting",
                        changed.len()
                    ));
                }
            } else if formatted == 0 {
                out.println("All BUILD files already formatted");
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

/// Render a BUILD file path as a workspace-relative label for output.
fn file_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let root = get_root()?;
    let matcher = match args.matcher {
        Some(s) => htpkg::parse(&s, &get_cwp()?)?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = BuildFmtApp {
        engine,
        matcher,
        root,
        check: args.check,
        indent: args.indent,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
