//! Ignored end-to-end tests for the `go` plugin: build a real `Engine` and run
//! a go target through it. Relocated from the `heph-plugin-go` crate (which
//! can't depend on the concrete `Engine`); kept here in the bin so it can.
//! Run with `cargo test -p heph -- --ignored`.

#[cfg(test)]
mod tests {
    use crate::engine::{Config, Engine, OutputMatcher, ResultOptions};
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;
    use crate::plugingo::Provider;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn copy_fixture(name: &str) -> tempfile::TempDir {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("crates/heph-plugin-go/src/plugingo/testdata")
            .join(name);
        let tmp = tempfile::tempdir().unwrap();
        copy_dir_all(&src, tmp.path()).unwrap();
        tmp
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
            } else {
                std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    fn make_addr(package: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(package), name.to_string(), Default::default())
    }

    fn build_test_engine(workspace: &std::path::Path) -> Arc<Engine> {
        let mut e = Engine::new(Config {
            root: workspace.to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .unwrap();

        e.register_provider(|_| Box::new(crate::pluginhostbin::Provider))
            .unwrap();
        e.register_driver(|_| Box::new(crate::pluginhostbin::Driver))
            .unwrap();
        e.register_managed_driver(|_| Box::new(crate::pluginexec::Driver::new_sh()))
            .unwrap();
        e.register_managed_driver(|_| Box::new(crate::pluginexec::Driver::new_exec()))
            .unwrap();
        e.register_provider(|init| Box::new(Provider::new(init.root.to_path_buf()).unwrap()))
            .unwrap();

        Arc::new(e)
    }

    #[tokio::test]
    #[ignore]
    async fn test_simple_lib_build_lib_e2e() {
        let sandbox = copy_fixture("simple_lib");
        let engine = build_test_engine(sandbox.path());
        let rs = engine.new_state();
        let addr = make_addr("", "build_lib");

        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
            .await
            .unwrap();

        assert!(
            !result.artifacts.is_empty(),
            "build_lib should produce at least one artifact"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_with_dep_cmd_build_e2e() {
        let sandbox = copy_fixture("with_dep");
        let engine = build_test_engine(sandbox.path());
        let rs = engine.new_state();
        let addr = make_addr("cmd", "build");

        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
            .await
            .unwrap();

        assert!(
            !result.artifacts.is_empty(),
            "cmd build should produce at least one artifact"
        );
    }
}
