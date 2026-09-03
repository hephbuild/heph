//! Parsers shared by the gates that read this repo's own build configuration.
//!
//! `tests/lint_gate.rs` and `tests/coverage_gate.rs` both assert *selection*
//! invariants — which code a gate is pointed at, and whether what it finds can
//! turn a check red — and both get there by reading `devenv.nix` and
//! `.github/workflows/heph.yml` as text. They shared these parsers by
//! copy-paste for exactly one commit; a second copy of a YAML-shaped parser
//! diverges quietly, and a gate reading a stale one is a gate that passes over
//! something it can no longer see.
//!
//! Everything here is a read: a few file reads and some string work, no build.
//! `expect`, never `panic!` — `clippy.toml`'s `allow-panic-in-tests` covers
//! `#[test]` bodies only, and these are free functions.

use std::path::{Path, PathBuf};

/// The repo root — these gates belong to the root package, so its manifest
/// dir *is* the root.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// The body of a `scripts.<name>.exec = ''…'';` block in `devenv.nix`.
///
/// Split per script rather than scanning the whole file: `lint` and `fix` both
/// contain `cargo clippy` lines and the invariant is that they *agree*, which
/// cannot be checked if the two are read as one pile. Same for `tst` and
/// `cov`.
///
/// `expect`, not `panic!`: `clippy.toml`'s `allow-panic-in-tests` only covers
/// `#[test]` bodies, and this is a free function.
#[track_caller]
pub fn devenv_script(devenv: &str, name: &str) -> String {
    let marker = format!("scripts.{name}.exec = ''");
    // Built up front rather than inside `expect`, which would be
    // `expect_fun_call`.
    let opened = format!("devenv.nix defines `scripts.{name}.exec` as a '' block");
    let closed = format!("`scripts.{name}.exec` block is closed");
    let (_, rest) = devenv.split_once(&marker).expect(&opened);
    let (body, _) = rest.split_once("'';").expect(&closed);
    body.to_string()
}

/// A job id and the text of its body, for every job in the workflow.
///
/// A job id is a two-space-indented `<id>:` alone on its line, below the
/// top-level `jobs:` key. Anchored on `jobs:` rather than matched everywhere,
/// because `on:` has `push:` and `pull_request:` at the same indent and they
/// are not jobs.
pub fn workflow_jobs(workflow: &str) -> Vec<(&str, String)> {
    let (_, body) = workflow
        .split_once("\njobs:\n")
        .expect("the workflow has a top-level `jobs:` key");

    let mut jobs: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in body.lines() {
        let header = line
            .strip_prefix("  ")
            .filter(|rest| !rest.starts_with([' ', '#', '-']))
            .and_then(|rest| rest.strip_suffix(':'))
            .filter(|id| {
                !id.is_empty()
                    && id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        match header {
            Some(id) => jobs.push((id, Vec::new())),
            None => {
                if let Some((_, lines)) = jobs.last_mut() {
                    lines.push(line);
                }
            }
        }
    }
    jobs.into_iter()
        .map(|(id, lines)| (id, lines.join("\n")))
        .collect()
}

/// The `needs:` of a job body, tokenised.
///
/// Tokenised rather than substring-matched: `lint` is a prefix of
/// `lint_matrix`, so a `contains` check reports `lint` present when only
/// `lint_matrix` is.
pub fn job_needs(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .find(|line| line.starts_with("needs:"))
        .map(|line| {
            line.strip_prefix("needs:")
                .unwrap_or(line)
                .trim()
                .trim_matches(['[', ']'])
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The `run:` step commands in a job body.
pub fn job_run_steps(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("run:"))
        .map(str::trim)
        .collect()
}

/// The `matrix.include` entries of a job body, as `<key>: <value>` maps.
///
/// Parsed rather than pattern-matched on `os:`/`arch:` so the platform
/// assertions read the same coordinates the workflow does.
pub fn matrix_include(body: &str) -> Vec<Vec<(String, String)>> {
    let mut entries: Vec<Vec<(String, String)>> = Vec::new();
    let mut in_include = false;
    for line in body.lines() {
        if line.trim() == "include:" {
            in_include = true;
            continue;
        }
        if !in_include {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A new entry starts at `- key: value`; `steps:` ends the block.
        let (starts_entry, pair) = match trimmed.strip_prefix("- ") {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        if starts_entry {
            entries.push(Vec::new());
        } else if entries.is_empty() || !line.starts_with("            ") {
            break;
        }
        if let Some((key, value)) = pair.split_once(':')
            && let Some(last) = entries.last_mut()
        {
            last.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    entries
}

/// The step blocks of a job body, each starting at a `      - ` line.
///
/// Some invariants are per *step*, not per job: the `kache stats` step
/// legitimately carries `if: always()`, so "the Lint step has no `if:`" cannot
/// be checked by scanning the whole job.
pub fn job_steps(body: &str) -> Vec<String> {
    let mut steps: Vec<Vec<&str>> = Vec::new();
    let mut in_steps = false;
    for line in body.lines() {
        if line.trim() == "steps:" {
            in_steps = true;
            continue;
        }
        if !in_steps {
            continue;
        }
        if line.starts_with("      - ") {
            steps.push(vec![line]);
        } else if let Some(last) = steps.last_mut() {
            last.push(line);
        }
    }
    steps.into_iter().map(|lines| lines.join("\n")).collect()
}

/// `text` with whole-line YAML comments removed.
///
/// A content assertion must not be satisfiable by prose. `workflow_jobs`
/// attributes a job's *leading* comment block to the job above it, so a
/// comment that merely mentions `needs.<dep>.result` would otherwise satisfy
/// the assertion that the aggregator checks it — reordering the two jobs is
/// enough to hand the aggregator that block and let it pass with its script
/// gutted.
pub fn without_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Job ids whose `run:` steps invoke `script`.
///
/// Derived from the `run:` steps rather than from the job name: a job that
/// keeps the name and the runner while running something narrower is the
/// regression, not the thing to trust. Comments are stripped first — a job's
/// leading comment block is attributed to it, and prose naming the script must
/// not be able to stand in for running it.
pub fn script_legs<'a>(jobs: &[(&'a str, String)], script: &str) -> Vec<&'a str> {
    jobs.iter()
        .filter(|(_, body)| job_run_steps(&without_comments(body)).contains(&script))
        .map(|(id, _)| *id)
        .collect()
}

/// The `<os>/<arch>` coordinates of a job body's `matrix.include`.
pub fn matrix_coords(body: &str) -> Vec<String> {
    matrix_include(body)
        .iter()
        .filter_map(|entry| {
            let get = |k: &str| {
                entry
                    .iter()
                    .find(|(key, _)| key == k)
                    .map(|(_, value)| value.clone())
            };
            Some(format!("{}/{}", get("os")?, get("arch")?))
        })
        .collect()
}
