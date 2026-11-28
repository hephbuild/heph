use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, InputMode, TargetDef, path::Content};
use crate::engine::request_state::RequestState;
use anyhow::Context as _;
use hcore::hartifactcontent::{Content as ArtifactContent, PathFilter};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct LinkedTargetDefInput {
    pub target: Arc<TargetDef>,
    pub output_names: Vec<String>,
    pub origin_id: String,
    pub filters: Vec<String>,
    pub annotations: std::collections::BTreeMap<String, String>,
    /// The outputs of a tool whose declaration cannot say how many files
    /// they hold (a glob or a directory, and no file): linking cannot hold
    /// them to "exactly 1 file", so the run does, once their artifacts
    /// exist (`Engine::inputs_result_exec`). Empty for anything but a tool.
    /// A cache hit skips the count, soundly: it is a function of the tool's
    /// entry paths, which its hashouts (folded into the consumer's hashin)
    /// cover.
    pub tool_outputs_counted_at_run: Vec<String>,
}

#[derive(Clone)]
pub struct LinkedTargetDef {
    pub target: Arc<TargetDef>,
    pub inputs: Vec<LinkedTargetDefInput>,
}

impl Engine {
    pub async fn link(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: Arc<TargetDef>,
    ) -> anyhow::Result<LinkedTargetDef> {
        // Expansion resolved every input's def on this request (and recorded
        // the dependency edges doing it), so linking reads them from there.
        let expanded_inputs = Arc::clone(&self)
            .expanded_inputs_for(rs.clone(), &def.addr)
            .await?;

        // Only `runtime=true` inputs get materialized into the sandbox.
        // `hash_deps` (runtime=false) are resolved for their hashout via meta
        // but are not extracted nor wired through SRC_*/LIST_*.
        let inputs = expanded_inputs
            .with_defs()
            .filter(|(input, _)| input.runtime)
            .map(|(input, input_def)| link_input(input, &input_def.target_def));
        let inputs = crate::engine::fanout::collect_failable(inputs, rs.fail_fast())?;

        Ok(LinkedTargetDef {
            target: def,
            inputs,
        })
    }
}

/// `input` linked to its def: the outputs it selects, checked to exist, and
/// for a tool, checked to be one file each.
fn link_input(input: &Input, input_def: &Arc<TargetDef>) -> anyhow::Result<LinkedTargetDefInput> {
    let output_names = if let Some(ref output_name) = input.r#ref.output {
        if !input_def
            .outputs
            .iter()
            .any(|output| &output.group == output_name)
        {
            anyhow::bail!(
                "Output '{output_name}' not found in target '{}'",
                input.r#ref.r#ref
            )
        }
        vec![output_name.clone()]
    } else {
        input_def.output_names()
    };

    let mut tool_outputs_counted_at_run = Vec::new();
    if input.mode == InputMode::Tool {
        // Each selected output must resolve to exactly 1 file.
        // Multi-output tool targets (e.g. a `nix` env exposing
        // `node`/`npm`/`npx`/`yarn` as 4 separate outputs) are fine:
        // pluginexec symlinks every produced file into the bin dir,
        // so each output's single file becomes one entry on PATH.
        for output in input_def
            .outputs
            .iter()
            .filter(|o| output_names.contains(&o.group))
        {
            let file_count = output
                .paths
                .iter()
                .filter(|p| matches!(p.content, Content::FilePath(_)))
                .count();
            // A glob or a directory holds however many files the run
            // produces: a filtering or relocating `group` declares only
            // globs, an `archive.extract` only its directory. The count is
            // checked against the artifacts instead.
            if file_count == 0
                && output
                    .paths
                    .iter()
                    .any(|p| matches!(p.content, Content::Glob(_) | Content::DirPath(_)))
            {
                tool_outputs_counted_at_run.push(output.group.clone());
                continue;
            }
            if file_count != 1 {
                anyhow::bail!(
                    "tool target '{}'{} must produce exactly 1 file, found {file_count}{}",
                    input.r#ref.r#ref,
                    output_clause(&output.group),
                    if file_count == 0 {
                        ""
                    } else {
                        ": select one with a `group` whose `include` keeps only it"
                    },
                );
            }
        }
    }

    Ok(LinkedTargetDefInput {
        target: Arc::clone(input_def),
        output_names,
        origin_id: input.origin_id.clone(),
        filters: input.r#ref.filters.clone(),
        annotations: input.annotations.clone(),
        tool_outputs_counted_at_run,
    })
}

/// The run-time half of the tool rule for an output linking could not count
/// (see [`LinkedTargetDefInput::tool_outputs_counted_at_run`]): `artifacts`,
/// that output's, hold exactly 1 file the reference's `filter` exposes — the
/// files staging will place.
///
/// Reads artifact indexes (a cache blob, a zstd frame): call it off the
/// async workers.
pub(crate) fn check_tool_output_files(
    tool: &hmodel::htaddr::Addr,
    output: &str,
    artifacts: &[Arc<dyn ArtifactContent>],
    filter: &PathFilter,
) -> anyhow::Result<()> {
    let files = exposed_entry_paths(artifacts, filter)
        .with_context(|| format!("listing the files of tool '{}'", tool.format()))?;
    if files.len() == 1 {
        return Ok(());
    }
    let tool = tool.format();
    let hint = if files.is_empty() {
        "the tool selected no file. For a `group`, its `include` matched nothing: it matches \
         a dep's paths as the dep's definition writes them (`<out>/…`) or workspace-relative \
         (`<package>/<out>/…`), and `heph inspect outputs` on the dep lists them"
            .to_owned()
    } else {
        // Name a few, so a too-wide `include` shows what it caught.
        const SHOWN: usize = 3;
        let mut listed = files
            .iter()
            .take(SHOWN)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if files.len() > SHOWN {
            write!(listed, " and {} more", files.len() - SHOWN)?;
        }
        format!(
            "{listed}. Narrow it to one file with a group `include` or a `[filter]` on the \
             tool reference; `heph inspect outputs {tool}` lists them"
        )
    };
    anyhow::bail!(
        "tool target '{tool}'{} must produce exactly 1 file, found {}: {hint}",
        output_clause(output),
        files.len(),
    )
}

/// The ` output '<name>'` clause of a tool error, empty for the default
/// (unnamed) output, which has no name to show.
fn output_clause(output: &str) -> String {
    if output.is_empty() {
        String::new()
    } else {
        format!(" output '{output}'")
    }
}

/// Every file and symlink path across `artifacts` that `filter` exposes,
/// sorted. Reads artifact indexes, so it blocks.
pub(crate) fn exposed_entry_paths(
    artifacts: &[Arc<dyn ArtifactContent>],
    filter: &PathFilter,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for artifact in artifacts {
        let entries = artifact
            .entry_paths()
            .context("listing an artifact's files")?;
        paths.extend(entries.into_iter().filter(|p| filter.matches(p)));
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::TargetAddr;
    use crate::engine::driver::targetdef::{
        Output,
        path::{CodegenMode, Path},
    };

    fn tool_input() -> Input {
        Input {
            r#ref: TargetAddr::parse("//p:t", &hmodel::htpkg::PkgBuf::from("p")).expect("addr"),
            mode: InputMode::Tool,
            origin_id: "tool:0".to_string(),
            annotations: Default::default(),
            hashed: true,
            runtime: true,
        }
    }

    fn def_with(paths: Vec<Content>) -> Arc<TargetDef> {
        Arc::new(TargetDef {
            outputs: vec![Output {
                group: String::new(),
                paths: paths
                    .into_iter()
                    .map(|content| Path {
                        content,
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    })
                    .collect(),
            }],
            ..Default::default()
        })
    }

    /// A glob or a directory and no file — a transforming `group`, an
    /// `archive.extract` — is counted at run, not refused at link.
    #[test]
    fn a_tool_output_of_globs_or_dirs_is_counted_at_run() {
        for paths in [
            vec![Content::Glob("p/**".into())],
            vec![Content::DirPath("p/d".into())],
        ] {
            let linked = link_input(&tool_input(), &def_with(paths)).expect("links");
            assert_eq!(linked.tool_outputs_counted_at_run, vec![String::new()]);
        }
    }

    /// What linking can count, it still refuses: no path at all, or two
    /// files, a glob beside them or not.
    #[test]
    fn a_tool_output_linking_can_count_is_held_at_link() {
        for paths in [
            vec![],
            vec![
                Content::FilePath("p/a".into()),
                Content::FilePath("p/b".into()),
            ],
            vec![
                Content::Glob("p/**".into()),
                Content::FilePath("p/a".into()),
                Content::FilePath("p/b".into()),
            ],
        ] {
            let err = link_input(&tool_input(), &def_with(paths))
                .err()
                .expect("refused at link")
                .to_string();
            assert!(err.contains("exactly 1 file, found"), "{err}");
            // The default output has no name to show.
            assert!(!err.contains("output ''"), "{err}");
        }
        for paths in [
            vec![Content::FilePath("p/a".into())],
            // One file declared: linking counts it, whatever the glob adds.
            vec![
                Content::Glob("p/**".into()),
                Content::FilePath("p/a".into()),
            ],
        ] {
            let linked = link_input(&tool_input(), &def_with(paths)).expect("links");
            assert!(linked.tool_outputs_counted_at_run.is_empty());
        }
    }

    /// A [`ArtifactContent`] that only lists paths.
    struct Listed(Vec<&'static str>);
    impl ArtifactContent for Listed {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            Ok(Box::new(std::io::empty()))
        }
        fn walk(
            &self,
        ) -> anyhow::Result<
            Box<dyn Iterator<Item = anyhow::Result<hcore::hartifactcontent::WalkEntry>> + '_>,
        > {
            Ok(Box::new(std::iter::empty()))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("listed".to_string())
        }
        fn entry_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
            Ok(self.0.iter().map(PathBuf::from).collect())
        }
    }

    fn count(
        artifacts: Vec<Vec<&'static str>>,
        output: &str,
        filters: &[&str],
    ) -> anyhow::Result<()> {
        let artifacts: Vec<Arc<dyn ArtifactContent>> = artifacts
            .into_iter()
            .map(|paths| Arc::new(Listed(paths)) as Arc<dyn ArtifactContent>)
            .collect();
        let filters: Vec<String> = filters.iter().map(|f| f.to_string()).collect();
        check_tool_output_files(
            &hmodel::htaddr::parse_addr("//p:t").expect("addr"),
            output,
            &artifacts,
            &PathFilter::new(&filters),
        )
    }

    /// Too many files: the first three, sorted across artifacts, and how
    /// many more; the named output is shown, with the hint.
    #[test]
    fn the_run_time_count_lists_three_sorted_and_the_rest_as_a_number() {
        let err = count(
            vec![vec!["p/e", "p/c"], vec!["p/a", "p/d", "p/b"]],
            "bin",
            &[],
        )
        .expect_err("5 files")
        .to_string();
        assert!(
            err.contains(
                "tool target '//p:t' output 'bin' must produce exactly 1 file, found 5: \
                 p/a, p/b, p/c and 2 more. Narrow it to one file"
            ),
            "{err}"
        );
    }

    /// The reference's filter is applied as staging applies it, and none
    /// left is the no-file hint.
    #[test]
    fn the_run_time_count_sees_what_the_filter_exposes() {
        count(vec![vec!["p/a", "p/b"]], "", &["p/b"]).expect("one file exposed");
        count(vec![vec!["p/a", "p/b/x"]], "", &["p/b/*"]).expect("one file exposed");
        let err = count(vec![vec!["p/a"]], "", &["p/z"])
            .expect_err("nothing exposed")
            .to_string();
        assert!(
            err.starts_with("tool target '//p:t' must produce exactly 1 file, found 0: the tool"),
            "{err}"
        );
        assert!(err.contains("`include` matched nothing"), "{err}");
    }
}
