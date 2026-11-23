use crate::tui::session::SessionSlot;
use std::collections::BTreeSet;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use hxstarlark_fmt::{Config, Reformat};

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::package_matcher_or_all;
use crate::engine::get_root;
use crate::pluginbuildfile::{FormatSettings, build_files_in_dir};
use crate::tui::LogSink;

/// Points at the cached targets that apply the same rule (docs/BUILDFILE_CHECKS.md).
const AFTER_HELP: &str = "\
Every package also has cached targets applying the same rule:
  heph run //<pkg>:build-format         rewrite the package's BUILD files
  heph run format-check //...           check every package, as CI does";

/// The argument value that selects stdin → stdout mode.
const STDIN_MARKER: &str = "-";

#[derive(clap::Args, Clone)]
#[command(
    override_usage = "heph tool build-fmt\n       heph tool build-fmt <PACKAGE_MATCHER>\n       heph tool build-fmt -",
    after_help = AFTER_HELP
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
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    if args.matcher.as_deref() == Some(STDIN_MARKER) {
        return format_stdin();
    }
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

/// Read Starlark from stdin, write the formatted result to stdout. The indent
/// width comes from the workspace's buildfile config when run inside one.
fn format_stdin() -> anyhow::Result<()> {
    let indent = FormatSettings::resolve(get_root().ok().as_deref()).indent;

    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .context("reading stdin")?;

    // A skipped or already formatted file is emitted unchanged, matching
    // in-place behavior.
    let out = decide(
        "<stdin>",
        &src,
        Config {
            indent_size: indent,
        },
    )?
    .unwrap_or(src);

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(out.as_bytes()).context("writing stdout")?;
    lock.flush().context("flushing stdout")?;
    Ok(())
}

/// What `build-fmt` does with one file whose source is `src`: the formatted
/// bytes to write (and, under `--check`, the file to report), or `None` to
/// leave it. The rule the `build-format-check` target applies too: a
/// skip-file directive counts as formatted, and a file that doesn't parse
/// fails the run, named, rather than being hidden.
fn decide(name: &str, src: &str, cfg: Config) -> anyhow::Result<Option<String>> {
    Ok(match hxstarlark_fmt::reformat(name, src, cfg)? {
        Reformat::Unchanged => None,
        Reformat::Changed(text) => Some(text),
    })
}

/// Render a BUILD file path as a workspace-relative label for output.
fn file_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

async fn execute_async(
    args: Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let root = get_root()?;
    let matcher = package_matcher_or_all(args.matcher.as_deref())?;
    let check = args.check;
    run_engine_command(
        "Formatting BUILD files",
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            let res: anyhow::Result<()> = async {
                let packages: Vec<String> = Arc::clone(&engine).packages(&matcher, &rs).await?;

                // The workspace's configured BUILD-file patterns (literal names like
                // `BUILD` and globs like `*.BUILD`) and indent width, so every file
                // the buildfile provider treats as a BUILD file is covered and
                // formatted as the workspace configures.
                let settings = FormatSettings::resolve(Some(&root));
                let cfg = Config {
                    indent_size: settings.indent,
                };

                // A package may have several matching files; dedup across packages.
                let mut build_files: BTreeSet<PathBuf> = BTreeSet::new();
                for pkg in &packages {
                    let dir = root.join(pkg);
                    build_files.extend(build_files_in_dir(&dir, &settings.patterns));
                }

                let mut changed = 0usize;
                let mut formatted = 0usize;

                for path in &build_files {
                    let label = file_label(&root, path);
                    let src = std::fs::read_to_string(path)
                        .with_context(|| format!("reading {}", path.display()))?;

                    let Some(out_text) = decide(&path.to_string_lossy(), &src, cfg)? else {
                        continue;
                    };

                    if check {
                        tracing::warn!("{label} is not formatted");
                        changed += 1;
                    } else {
                        std::fs::write(path, &out_text)
                            .with_context(|| format!("writing {}", path.display()))?;
                        tracing::info!("formatted {label}");
                        formatted += 1;
                    }
                }

                if check {
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
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{Config, decide};
    use std::path::PathBuf;

    /// Every formatter fixture's input and expected output.
    fn fixtures() -> Vec<(String, String)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/xstarlark-fmt/testdata");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read testdata") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .to_string();
            let content = std::fs::read_to_string(&path).expect("read fixture");
            // Work-in-progress fixtures, which the golden test skips too.
            if content.contains("# skip") {
                continue;
            }
            let (input, expected) = content.split_once("----\n").expect("separator");
            out.push((format!("{name} input"), input.to_owned()));
            out.push((format!("{name} expected"), expected.to_owned()));
        }
        assert!(
            out.len() > 20,
            "found almost no fixtures in {}",
            dir.display()
        );
        out
    }

    /// `heph tool build-fmt --check` reports a file exactly when the
    /// `build-format-check` target fails it, at every indent.
    #[test]
    fn check_matches_build_fmt() {
        let mut cases = fixtures();
        cases.push((
            "skip-file".into(),
            "# heph:fmt skip-file\nx    =  1\n".into(),
        ));
        cases.push(("unformatted".into(), "x    =  1\n".into()));
        let mut flagged = 0;
        for indent_size in [2, 4] {
            for (name, src) in &cases {
                let build_fmt = decide("pkg/BUILD", src, Config { indent_size }).expect("parses");
                let check =
                    crate::pluginbuildfile::format_check_verdict("pkg/BUILD", src, indent_size)
                        .expect("parses");
                assert_eq!(
                    check.is_some(),
                    build_fmt.is_some(),
                    "{name} (indent {indent_size}): build-fmt {build_fmt:?}, check {check:?}"
                );
                if let Some(line) = check {
                    flagged += 1;
                    assert!(
                        line.starts_with("pkg/BUILD:")
                            && line.contains(":1: format: not formatted"),
                        "{name}: {line}"
                    );
                }
            }
        }
        assert!(
            flagged > 0,
            "no case needed formatting, so nothing was compared"
        );
    }
}
