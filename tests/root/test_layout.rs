//! The merged integration-test binaries must still compile every test file.
//!
//! `crates/e2e` and the root package link their integration tests into one
//! binary each (`tests/e2e/main.rs`, `tests/root/main.rs`), because every
//! `tests/*.rs` binary linked all of `heph` again. The cost of that layout is a
//! silent hole: a `.rs` file dropped into the aggregated directory and never
//! named by a `mod` line is not compiled, and its tests never run — nothing
//! goes red. And a new top-level `tests/*.rs` brings back a whole link of
//! `heph` without anyone deciding it should.
//!
//! So both directions are asserted: every file in an aggregated directory is
//! declared, and every top-level test file is on the list of deliberate
//! separate binaries (each of which touches process-global state; see the
//! `main.rs` docs for why each one is there).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A package's `tests/` directory, the aggregated binary inside it, and the
/// top-level files that stay separate binaries on purpose.
struct Layout {
    tests_dir: &'static str,
    aggregated: &'static str,
    separate: &'static [&'static str],
    /// Subdirectories that are shared support code, not a test binary.
    support: &'static [&'static str],
}

const LAYOUTS: &[Layout] = &[
    Layout {
        tests_dir: "tests",
        aggregated: "root",
        separate: &[
            "supervisor.rs",
            "handle_drop_reaps.rs",
            "wait_with_output_pipe.rs",
            "process_watcher_stress.rs",
        ],
        support: &[],
    },
    Layout {
        tests_dir: "crates/e2e/tests",
        aggregated: "e2e",
        separate: &["credential.rs", "oci_credential.rs", "deferred.rs"],
        support: &["common"],
    },
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// The module names a `main.rs` declares with `mod <name>;`.
fn declared_modules(main_rs: &str) -> BTreeSet<String> {
    main_rs
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            line.strip_prefix("pub ")
                .unwrap_or(line)
                .strip_prefix("mod ")?
                .strip_suffix(';')
                .map(|name| name.trim().to_string())
        })
        .collect()
}

/// `(files, dirs)` directly inside `dir`, by name.
fn entries(dir: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut files = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    for entry in std::fs::read_dir(dir).expect("read a tests directory") {
        let entry = entry.expect("read_dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type().expect("file type");
        if kind.is_dir() {
            dirs.insert(name);
        } else if name.ends_with(".rs") {
            files.insert(name);
        }
    }
    (files, dirs)
}

#[test]
fn every_test_file_is_compiled() {
    let root = repo_root();
    for layout in LAYOUTS {
        let tests_dir = root.join(layout.tests_dir);
        let aggregated = tests_dir.join(layout.aggregated);
        let main_rs = std::fs::read_to_string(aggregated.join("main.rs"))
            .unwrap_or_else(|e| panic!("read {}/main.rs: {e}", aggregated.display()));
        let declared = declared_modules(&main_rs);

        // Every file and module directory in the aggregated directory is
        // named by a `mod` line in its `main.rs`.
        let (files, dirs) = entries(&aggregated);
        let undeclared: Vec<String> = files
            .iter()
            .filter(|f| *f != "main.rs")
            .filter_map(|f| f.strip_suffix(".rs"))
            .chain(
                dirs.iter()
                    .filter(|d| aggregated.join(d).join("mod.rs").is_file())
                    .map(String::as_str),
            )
            .filter(|m| !declared.contains(*m))
            .map(str::to_string)
            .collect();
        assert!(
            undeclared.is_empty(),
            "{}: these files are not declared by main.rs, so they are never compiled \
             and their tests never run: {undeclared:?}. Add `mod <name>;` for each.",
            aggregated.display()
        );

        // Every top-level test file is a deliberate separate binary.
        let (files, dirs) = entries(&tests_dir);
        let unlisted: Vec<&String> = files
            .iter()
            .filter(|f| !layout.separate.contains(&f.as_str()))
            .collect();
        assert!(
            unlisted.is_empty(),
            "{}: {unlisted:?} would each be a separate test binary linking all of heph. \
             Move them into `{}/` and declare them in its main.rs — or, if one touches \
             process-global state (set_var, signals, reaping, re-exec), add it to the \
             separate-binary list in this test and say why in main.rs.",
            tests_dir.display(),
            layout.aggregated
        );
        let stale: Vec<&&str> = layout
            .separate
            .iter()
            .filter(|f| !files.contains(**f))
            .collect();
        assert!(
            stale.is_empty(),
            "{}: the separate-binary list names files that no longer exist: {stale:?}",
            tests_dir.display()
        );

        // A subdirectory with a `main.rs` is another test binary.
        let extra_binaries: Vec<&String> = dirs
            .iter()
            .filter(|d| *d != layout.aggregated && !layout.support.contains(&d.as_str()))
            .filter(|d| tests_dir.join(d).join("main.rs").is_file())
            .collect();
        assert!(
            extra_binaries.is_empty(),
            "{}: {extra_binaries:?} are further test binaries next to `{}/`; merge them into it",
            tests_dir.display(),
            layout.aggregated
        );
    }
}

#[test]
fn declared_modules_reads_plain_and_pub_mod_lines() {
    let main_rs = "//! docs mention mod nothing;\n#[path = \"../common/mod.rs\"]\nmod common;\npub mod a;\n  mod b;\nuse c;\n";
    let got: Vec<String> = declared_modules(main_rs).into_iter().collect();
    assert_eq!(got, ["a", "b", "common"]);
}
