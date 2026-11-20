//! Fixtures for the load-walk tests: a temp workspace, the walk over it, and
//! evaluation of the same package to hold the walk against.

use super::loads::{LoadWalk, walk_package};
use crate::pluginbuildfile::provider::{PackageList, default_build_file_patterns};
use crate::pluginbuildfile::run_file::{BuildFileLoader, RunResult};
use hplugin::function::FunctionRegistry;
use hwalk::CachedWalker;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;

/// A workspace holding `files` (workspace-relative path, content).
pub(super) fn ws(files: &[(&str, &str)]) -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (path, content) in files {
        let abs = tmp.path().join(path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("create dirs");
        }
        std::fs::write(&abs, content).expect("write fixture");
    }
    tmp
}

/// The load walk of `pkg`, with the default BUILD patterns.
pub(super) fn walk(root: &Path, pkg: &str) -> anyhow::Result<LoadWalk> {
    walk_package(
        root,
        &CachedWalker::disabled(),
        pkg,
        &default_build_file_patterns(),
    )
}

/// Evaluate `pkg` as the provider does, returning every file the loader
/// evaluated (workspace-relative): the package's own BUILD files and every
/// file a `load()` reached, read off a file cache no other evaluation shares.
pub(super) fn eval(root: &Path, pkg: &str) -> anyhow::Result<BTreeSet<String>> {
    let patterns = default_build_file_patterns();
    let walker = Arc::new(CachedWalker::disabled());
    let file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>> = Arc::default();
    let loader = BuildFileLoader::new(
        root.to_path_buf(),
        patterns.clone(),
        Arc::clone(&file_cache),
        Arc::default(),
        Arc::new(FunctionRegistry::default()),
        Arc::new(OnceLock::new()),
        Arc::clone(&walker),
        Arc::new(PackageList::new(
            root.to_path_buf(),
            patterns,
            Arc::new(hwalk::Ignore::default()),
            walker,
        )),
        Arc::default(),
    );
    loader.load_pkg(pkg)?;
    let opened = file_cache
        .lock()
        .expect("file cache")
        .keys()
        .map(|p| {
            p.strip_prefix(root)
                .expect("under root")
                .to_str()
                .expect("utf-8")
                .to_string()
        })
        .collect();
    Ok(opened)
}

/// Every message in `err`'s chain, joined.
pub(super) fn chain(err: &anyhow::Error) -> String {
    err.chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The walk's own failure, without the `<file>: load("…")` context it adds
/// on the way back up: the message evaluation must also carry.
pub(super) fn walk_failure(err: &anyhow::Error) -> String {
    err.chain()
        .map(ToString::to_string)
        .filter(|m| !m.contains(": load(\""))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The walk of `pkg` and its evaluation agree (I8): the same files, or both
/// failing, every message of the walk's failure carried by evaluation's.
/// Returns the walk's failure, if any, for a caller to pin.
pub(super) fn assert_matches_eval(root: &Path, pkg: &str, name: &str) -> Option<String> {
    match (walk(root, pkg), eval(root, pkg)) {
        (Ok(walk), Ok(opened)) => {
            let walked: BTreeSet<String> = walk.inputs().map(str::to_string).collect();
            assert_eq!(
                walked, opened,
                "{name}: walk and evaluation read different files"
            );
            None
        }
        (Err(walk_err), Err(eval_err)) => {
            let eval_text = chain(&eval_err);
            for message in walk_err
                .chain()
                .map(ToString::to_string)
                .filter(|m| !m.contains(": load(\""))
            {
                assert!(
                    eval_text.contains(&message),
                    "{name}: evaluation's error does not carry the walk's `{message}`\n\
                     walk: {}\neval: {eval_text}",
                    chain(&walk_err)
                );
            }
            Some(walk_failure(&walk_err))
        }
        (Ok(walk), Err(eval_err)) => panic!(
            "{name}: the walk succeeded ({:?}) where evaluation failed: {}",
            walk.inputs().collect::<Vec<_>>(),
            chain(&eval_err)
        ),
        (Err(walk_err), Ok(_)) => panic!(
            "{name}: the walk failed where evaluation succeeded: {}",
            chain(&walk_err)
        ),
    }
}
