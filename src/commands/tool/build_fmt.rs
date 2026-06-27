use std::collections::BTreeSet;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use hxstarlark_fmt::{Config, FmtError};

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::{Engine, get_cwp, get_root};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::pluginbuildfile::{FormatSettings, build_files_in_dir};
use crate::tui::{self, App, AppContext, LogSink};

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
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    if args.matcher.as_deref() == Some(STDIN_MARKER) {
        return format_stdin();
    }
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

/// Read Starlark from stdin, write the formatted result to stdout. The indent
/// width comes from the workspace's buildfile config when run inside one.
fn format_stdin() -> anyhow::Result<()> {
    let indent = FormatSettings::resolve(get_root().ok().as_deref()).indent;

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

        let res: anyhow::Result<()> = async {
            let packages: Vec<String> = Arc::clone(&self.engine)
                .packages(&self.matcher, &rs)
                .await?
                .collect::<anyhow::Result<Vec<_>>>()?;

            // The workspace's configured BUILD-file patterns (literal names like
            // `BUILD` and globs like `*.BUILD`) and indent width, so every file
            // the buildfile provider treats as a BUILD file is covered and
            // formatted as the workspace configures.
            let settings = FormatSettings::resolve(Some(&self.root));
            let cfg = Config {
                indent_size: settings.indent,
            };

            // A package may have several matching files; dedup across packages.
            let mut build_files: BTreeSet<PathBuf> = BTreeSet::new();
            for pkg in &packages {
                let dir = self.root.join(pkg);
                build_files.extend(build_files_in_dir(&dir, &settings.patterns));
            }

            let mut changed = 0usize;
            let mut formatted = 0usize;

            for path in &build_files {
                let label = file_label(&self.root, path);
                let src = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;

                let out_text = match hxstarlark_fmt::format(&path.to_string_lossy(), &src, cfg) {
                    Ok(text) => text,
                    // The `heph:fmt skip-file` directive: leave untouched.
                    Err(FmtError::Skip) => continue,
                    // A file that doesn't parse (a syntax error, or tab
                    // indentation, which heph's Starlark dialect rejects) can't
                    // be reformatted — fail the run so it's fixed, not hidden.
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
                    tracing::warn!("{label} is not formatted");
                    changed += 1;
                } else {
                    std::fs::write(path, &out_text)
                        .with_context(|| format!("writing {}", path.display()))?;
                    tracing::info!("formatted {label}");
                    formatted += 1;
                }
            }

            if self.check {
                if changed > 0 {
                    return Err(anyhow::anyhow!("{changed} BUILD file(s) need formatting"));
                }
                tracing::info!("all BUILD files are formatted");
            } else if formatted == 0 {
                tracing::info!("all BUILD files already formatted");
            } else {
                tracing::info!("formatted {formatted} BUILD file(s)");
            }
            Ok(())
        }
        .await;

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
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
