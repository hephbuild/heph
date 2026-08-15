//! Regression coverage for `Engine::new` canonicalizing `cfg.root`.
//!
//! Found live: `pluginjs-e2e`'s real end-to-end `js_test` suite reported "No
//! test files found" against a real `vitest` run, even though the test file
//! plainly existed in the sandbox. Root cause — nothing js-specific: on
//! macOS, `/tmp`/`/var` are symlinks to `/private/tmp`/`/private/var` (the
//! default `TMPDIR` `tempfile::tempdir()` resolves into), and `Engine::new`
//! used to keep the caller's root exactly as given. Every absolute path a
//! managed driver builds downstream (`sandbox_dir`/`sandbox_ws_dir`/
//! `sandbox_pkg_dir`, and anything a driver joins onto them — e.g. the
//! absolute test-file path `js_test` passes vitest as a CLI argument)
//! inherited that uncanonicalized prefix, but a real subprocess that does
//! its own filesystem walk (vitest's test-file discovery) resolves the
//! *same* file to its canonical path. The two strings never matched.
//!
//! Deliberately does not exec a subprocess and check its observed `pwd`:
//! `chdir(2)` followed by `getcwd(2)` *always* returns the physical
//! (symlink-resolved) path regardless of the string used to `chdir` there —
//! that would pass whether or not `Engine::new` canonicalizes anything,
//! silently proving nothing. Inspecting `Engine::home`/`Engine::root()`
//! directly is what actually discriminates the fix from its absence —
//! confirmed by reverting the fix locally: a `pwd -P` version of this test
//! stayed green either way, this one goes red.
//!
//! Checks both `home` *and* `root()`: a first version of this fix
//! canonicalized the local variable used to derive `home` but never wrote it
//! back into `cfg.root` itself, so `Engine::root()`/`PluginInit.root` (handed
//! to every provider/driver factory) and `RunRequest.tree_root_path` (the ABI
//! field crossing to every managed driver, including out-of-process cdylib
//! plugins) silently kept the raw, symlinked value — a code-quality review
//! caught it before it shipped. Asserting on `root()` here, not just `home`,
//! is what would have caught that gap.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use heph::engine::{Config, Engine};

/// Removes a symlink on drop — the test's own cleanup, since `link` is not
/// itself a `TempDir` (it has to be a plain symlink for the regression to
/// mean anything).
struct RemoveOnDrop(std::path::PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.0));
    }
}

#[tokio::test]
#[cfg(unix)]
async fn engine_new_canonicalizes_a_symlinked_root() -> anyhow::Result<()> {
    let real = tempfile::tempdir()?;
    let real_canonical = std::fs::canonicalize(real.path())?;

    // Named from the real dir's own (already-unique) tempdir name, so two
    // copies of this test running concurrently in the same process (cargo
    // test's default) never collide on the symlink's own path.
    let link = real.path().with_file_name(format!(
        "link-{}",
        real.path()
            .file_name()
            .expect("tempdir has a file name")
            .to_string_lossy()
    ));
    std::os::unix::fs::symlink(real.path(), &link)?;
    let _cleanup = RemoveOnDrop(link.clone());

    let engine = Engine::new(Config {
        root: link.clone(),
        home_dir: std::path::PathBuf::new(),
        ..Default::default()
    })?;

    assert_eq!(
        engine.home,
        real_canonical.join(".heph3"),
        "Engine::new must canonicalize a symlinked `cfg.root` — `home` (and everything a \
         managed driver builds from it: sandbox_dir/sandbox_ws_dir/sandbox_pkg_dir) still \
         carries the symlink's own literal path, {:?}, instead of the real one, {:?}",
        engine.home,
        real_canonical.join(".heph3"),
    );
    assert_eq!(
        engine.root(),
        real_canonical,
        "Engine::new must also write the canonicalized root back into `cfg.root` itself, not \
         just use a local for deriving `home` — `root()` (and PluginInit.root/\
         RunRequest.tree_root_path, which every provider/driver factory and every managed \
         driver's ABI request read it from) still carries the symlink's own literal path, {:?}, \
         instead of the real one, {:?}",
        engine.root(),
        real_canonical,
    );
    Ok(())
}
