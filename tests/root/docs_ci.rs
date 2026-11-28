//! `docs/CI.md` against the code it documents: the environment variables, the
//! config files and the commands its recipes name are the ones heph reads and
//! runs, and the trust warnings and the version pin a reader needs are there.
//!
//! The recipes are copied into CI configs as they stand, so a renamed
//! variable, key or subcommand fails here rather than in someone's pipeline,
//! where it would only make every job cold.

use heph::engine::config_yaml::{self, CACHE_SNAPSHOT_ENV, PROFILES_ENV};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The snapshot directory every recipe caches and every config example sets.
const SNAPSHOT_DIR: &str = ".heph-snapshot";

/// The CIs with a recipe, by their `###` heading.
const RECIPES: [&str; 4] = ["GitHub Actions", "GitLab CI", "CircleCI", "Buildkite"];

/// The command a recipe runs after the build.
const SNAPSHOT_COMMAND: &str = "heph tool cache snapshot";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_doc() -> String {
    std::fs::read_to_string(repo_root().join("docs").join("CI.md")).expect("reading docs/CI.md")
}

/// A fenced code block, with the `##`/`###` heading it sits under.
struct Block {
    heading: String,
    lang: String,
    body: String,
}

fn blocks(doc: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut open: Option<Block> = None;
    for line in doc.lines() {
        if let Some(block) = open.as_mut() {
            if line.trim_start().starts_with("```") {
                out.extend(open.take());
            } else {
                block.body.push_str(line);
                block.body.push('\n');
            }
        } else if let Some(lang) = line.trim_start().strip_prefix("```") {
            open = Some(Block {
                heading: heading.clone(),
                lang: lang.trim().to_string(),
                body: String::new(),
            });
        } else if let Some(h) = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "))
        {
            heading = h.trim().to_string();
        }
    }
    assert!(open.is_none(), "docs/CI.md ends inside a code block");
    out
}

/// Every `HEPH_…` name the doc mentions.
fn heph_env_names(doc: &str) -> Vec<&str> {
    doc.match_indices("HEPH_")
        .map(|(at, _)| {
            let rest = doc.get(at..).unwrap_or_default();
            let end = rest
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            rest.get(..end).unwrap_or_default()
        })
        .collect()
}

/// The `heph tool …` invocations in `body`, as argv after `heph`: from
/// `heph tool` up to the end of the line or the first quote or shell
/// separator.
fn heph_tool_invocations(body: &str) -> Vec<Vec<String>> {
    body.lines()
        .filter_map(|line| {
            let at = line.find("heph tool ")?;
            let rest = line.get(at + "heph ".len()..)?;
            let cmd = rest
                .split(['\'', '"', '`', ';', '|', '&', ')', '#'])
                .next()
                .unwrap_or_default();
            Some(cmd.split_whitespace().map(str::to_string).collect())
        })
        .collect()
}

/// `heph <args> --help` exits 0: clap knows every subcommand and flag in
/// `args`.
fn assert_cli_accepts(args: &[String]) {
    let cwd = tempfile::tempdir().expect("cwd");
    let home = tempfile::tempdir().expect("home");
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_heph")))
        .args(args)
        .arg("--help")
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("HEPH_CWD", cwd.path())
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .env("HEPH_NO_SESSION", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph");
    assert_eq!(
        out.status.code(),
        Some(0),
        "docs/CI.md runs `heph {}`, which this heph does not accept:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The config file a block shows, from its first line: `# ci.hephconfig`.
fn config_file_name(block: &Block) -> Option<&str> {
    let first = block.body.lines().next()?;
    let name = first.strip_prefix("# ")?.trim();
    name.ends_with(".hephconfig").then_some(name)
}

/// Every recipe in `docs/CI.md` uses the environment variables, the config
/// key and the command the code defines, and the I2 and configuration-2
/// trust warnings and the version pin are present.
#[test]
fn recipes_name_the_real_env_var() {
    let doc = read_doc();
    let blocks = blocks(&doc);

    // The environment: the two variables heph reads, and no other `HEPH_`
    // name a reader would set for nothing.
    for name in [CACHE_SNAPSHOT_ENV, PROFILES_ENV] {
        assert!(doc.contains(name), "docs/CI.md never names {name}");
    }
    for name in heph_env_names(&doc) {
        assert!(
            name == CACHE_SNAPSHOT_ENV || name == PROFILES_ENV,
            "docs/CI.md names {name}, which is not a variable the snapshot reads"
        );
    }

    // The config examples load as heph loads a config file, and every one
    // that turns snapshots on names the directory the recipes cache.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut sets_snapshot = false;
    let mut pins_version = false;
    for block in blocks.iter().filter(|b| b.lang == "yaml") {
        let Some(name) = config_file_name(block) else {
            continue;
        };
        let path = dir.path().join(name);
        std::fs::write(&path, &block.body).expect("write config");
        let cfg = config_yaml::load(&path).unwrap_or_else(|e| {
            panic!(
                "docs/CI.md's {name} under \"{}\" does not load: {e:#}\n{}",
                block.heading, block.body
            )
        });
        if let Some(snapshot) = &cfg.cache_snapshot {
            assert_eq!(
                snapshot.dir,
                Path::new(SNAPSHOT_DIR),
                "{name} under \"{}\"",
                block.heading
            );
            sets_snapshot = true;
        }
        pins_version |= cfg.version.is_some();
    }
    assert!(sets_snapshot, "no config example sets `cacheSnapshot:`");
    assert!(pins_version, "no config example pins a heph `version`");

    // Each recipe caches the configured directory, selects the profile and
    // runs the snapshot command.
    for ci in RECIPES {
        let recipe: String = blocks
            .iter()
            .filter(|b| b.heading == ci && b.lang == "yaml")
            .map(|b| b.body.as_str())
            .collect();
        assert!(!recipe.is_empty(), "docs/CI.md has no {ci} recipe");
        for needle in [SNAPSHOT_COMMAND, SNAPSHOT_DIR, PROFILES_ENV] {
            assert!(
                recipe.contains(needle),
                "the {ci} recipe does not name `{needle}`"
            );
        }
    }

    // Every `heph tool …` the doc shows is a command this binary accepts.
    let mut invocations: Vec<Vec<String>> = blocks
        .iter()
        .flat_map(|b| heph_tool_invocations(&b.body))
        .collect();
    invocations.sort();
    invocations.dedup();
    let snapshot_argv: Vec<String> = SNAPSHOT_COMMAND
        .split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect();
    assert!(
        invocations.contains(&snapshot_argv),
        "no code block runs `{SNAPSHOT_COMMAND}`"
    );
    for args in &invocations {
        assert_cli_accepts(args);
    }

    // The trust rule (I2) and configuration 2's warning, which the recipes
    // rest on, the version pin's section, and where diagnosis starts. Matched
    // with whitespace collapsed, so rewrapping the prose keeps them.
    let prose = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    for needle in [
        "## Trust",
        "A snapshot is trusted exactly like a remote cache you can write to.",
        "a ref that's less trusted must never write a cache entry that a ref that's more trusted restores.",
        "extends the bucket's trust to every writer of the snapshot's default-branch entries",
        "### Pin the heph version",
        "read which key the CI's restore step matched",
    ] {
        assert!(prose.contains(needle), "docs/CI.md lost `{needle}`");
    }
}
