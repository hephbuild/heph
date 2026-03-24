//! End-to-end CLI acceptance tests
//!
//! These tests validate the Rust CLI implementation against real BUILD files
//! from the example project. They test the complete build workflow from
//! CLI invocation through to successful build completion.

use std::process::Command;
use std::path::PathBuf;

fn get_cli_binary() -> PathBuf {
    // Get the path to the CLI binary
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // Go up from heph crate
    path.pop(); // Go up from crates
    path.push("target");

    // Use debug binary for faster test compilation
    if cfg!(debug_assertions) {
        path.push("debug");
    } else {
        path.push("release");
    }

    path.push("heph");
    path
}

fn get_example_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // Go up from heph crate
    path.pop(); // Go up from crates
    path.push("examples");
    path.push("build_files");
    path
}

fn run_heph_cli(args: &[&str]) -> std::process::Output {
    let cli_binary = get_cli_binary();
    let example_dir = get_example_dir();

    Command::new(&cli_binary)
        .args(args)
        .current_dir(&example_dir)
        .output()
        .expect("Failed to execute CLI")
}

#[test]
fn test_cli_simple_target() {
    // Test building a simple target with no dependencies
    let output = run_heph_cli(&["run", "//example:sanity"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Building Targets"), "Missing 'Building Targets' header");
    assert!(stdout.contains("//example:sanity"), "Missing target reference");
    assert!(stdout.contains("Build Summary"), "Missing build summary");
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
fn test_cli_target_with_dependencies() {
    // Test building a target with dependencies
    let output = run_heph_cli(&["run", "//simple_deps:result"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//simple_deps:result"), "Missing target reference");
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
fn test_cli_deep_dependencies() {
    // Test building a target with deep dependency chain
    let output = run_heph_cli(&["run", "//deep_deps:final"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//deep_deps:final"), "Missing target reference");
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
fn test_cli_named_dependencies() {
    // Test building a target with named dependencies
    let output = run_heph_cli(&["run", "//named_deps:final"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//named_deps:final"), "Missing target reference");
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
fn test_cli_multiple_targets() {
    // Test building multiple targets in one command
    let output = run_heph_cli(&["run", "//example:sanity", "//simple_deps:d1"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("//example:sanity"), "Missing first target");
    assert!(stdout.contains("//simple_deps:d1"), "Missing second target");
    assert!(stdout.contains("Successfully built 2 target(s)"), "Wrong target count");
}

#[test]
fn test_cli_parallel_jobs() {
    // Test setting parallel jobs
    let output = run_heph_cli(&["run", "--jobs", "8", "//example:sanity"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
fn test_cli_force_rebuild() {
    // Test force rebuild (cache disabled)
    let output = run_heph_cli(&["run", "--force", "//example:sanity"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
fn test_cli_verbose_output() {
    // Test verbose mode
    let output = run_heph_cli(&["run", "--verbose", "//example:sanity"]);

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Parallel jobs:"), "Missing verbose parallel jobs info");
    assert!(stdout.contains("Cache:"), "Missing verbose cache info");
}

#[test]
fn test_cli_invalid_target() {
    // Test error handling for invalid target reference
    let output = run_heph_cli(&["run", "invalid_target"]);

    assert!(
        !output.status.success(),
        "CLI should have failed for invalid target"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error") || stderr.contains("Invalid"),
        "Missing error message for invalid target"
    );
}

#[test]
fn test_cli_working_directory() {
    // Test changing working directory with -C flag
    let cli_binary = get_cli_binary();
    let example_dir = get_example_dir();

    let output = Command::new(&cli_binary)
        .args(&["-C", example_dir.to_str().unwrap(), "run", "//example:sanity"])
        .output()
        .expect("Failed to execute CLI");

    assert!(
        output.status.success(),
        "CLI failed with -C flag: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Successfully built"), "Missing success message");
}

#[test]
#[ignore] // This test requires building the CLI binary first
fn test_cli_help() {
    let cli_binary = get_cli_binary();

    let output = Command::new(&cli_binary)
        .args(&["--help"])
        .output()
        .expect("Failed to execute CLI");

    assert!(output.status.success(), "CLI help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Heph"), "Missing CLI name");
    assert!(stdout.contains("run"), "Missing run command");
    assert!(stdout.contains("query"), "Missing query command");
}

#[test]
#[ignore] // This test requires building the CLI binary first
fn test_cli_version() {
    let cli_binary = get_cli_binary();

    let output = Command::new(&cli_binary)
        .args(&["--version"])
        .output()
        .expect("Failed to execute CLI");

    assert!(output.status.success(), "CLI version should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("heph"), "Missing version info");
}
