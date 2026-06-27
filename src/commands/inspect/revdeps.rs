use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use futures::StreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::Engine;
use crate::htaddr::{self, Addr};
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target whose users to find: a `//pkg:name` or relative (`:name`,
    /// `./pkg:name`) address, or a path to an existing file (sugar for its `fs`
    /// file target)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Restrict the search to packages matching this matcher (e.g. //pkg/...);
    /// defaults to the whole workspace
    #[arg(long, value_name = "PACKAGE_MATCHER")]
    pub scope: Option<String>,
}

struct RevdepsApp {
    engine: Arc<Engine>,
    /// The target whose reverse dependencies (users) we want.
    addr: Addr,
    /// Packages to search for users (whole workspace by default).
    scope: Matcher,
    fail_fast: bool,
}

#[async_trait]
impl App for RevdepsApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Revdeps {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Revdeps {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let RevdepsApp {
            engine,
            addr,
            scope,
            fail_fast,
        } = self;
        let rs = engine.new_state_with_events(fail_fast, ctx.event_sender());

        // Stream dependents as they are found and print incrementally. Resolving
        // in-scope targets records rich failures in `rs`, which `finalize`
        // prefers over the returned error.
        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            let dependents = Arc::clone(&engine).revdeps(rs.clone(), addr, &scope);
            futures::pin_mut!(dependents);
            while let Some(dependent) = dependents.next().await {
                out.println(dependent?.format());
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

/// Resolve a CLI target argument into an `Addr`, relative to package `cwp` under
/// workspace `root`.
///
/// First tries to parse `input` as a (possibly relative) target address —
/// `//pkg:name`, `:name`, `./pkg:name`. If that fails, `input` is treated as a
/// path: when it points at an existing file, the file's `fs` target
/// (`//@heph/fs:file@f=<root-relative path>`) is used; otherwise the original
/// parse error is surfaced.
fn resolve_addr_in(input: &str, cwp: &PkgBuf, root: &Path) -> anyhow::Result<Addr> {
    match htaddr::parse_addr_with_base(input, cwp) {
        Ok(addr) => Ok(addr),
        Err(parse_err) => {
            // Not a valid address — fall back to the file-path sugar, but only
            // when it actually names a file on disk. Otherwise the address parse
            // error is the useful one to show.
            if let Ok(rel) = htpkg::join_rel_checked(cwp.as_str(), input)
                && root.join(&rel).is_file()
            {
                return Ok(crate::pluginfs::file_addr(&rel));
            }
            Err(parse_err).with_context(|| format!("parse {input}"))
        }
    }
}

/// `resolve_addr_in` against the current working package and workspace root.
fn resolve_addr(input: &str) -> anyhow::Result<Addr> {
    resolve_addr_in(
        input,
        &crate::engine::get_cwp()?,
        &crate::engine::get_root()?,
    )
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let addr = resolve_addr(args.addr.as_ref())?;
    let scope = match &args.scope {
        Some(s) => htpkg::parse(s.as_str(), &crate::engine::get_cwp()?)
            .with_context(|| format!("parse scope {s}"))?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RevdepsApp {
        engine,
        addr,
        scope,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::resolve_addr_in;
    use crate::htaddr::parse_addr;
    use crate::htpkg::PkgBuf;

    /// A tempdir root with `rel` touched as an empty file.
    fn root_with_file(rel: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        dir
    }

    #[test]
    fn existing_bare_file_resolves_to_fs_file_addr() {
        // A bare path is not a valid address, so it falls back to the file
        // sugar — the fs file target keyed by the root-relative path.
        let root = root_with_file("cmd/server/data.txt");
        let addr = resolve_addr_in("data.txt", &PkgBuf::from("cmd/server"), root.path()).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/data.txt"));
    }

    #[test]
    fn bare_subdir_file_resolves_against_package() {
        let root = root_with_file("cmd/server/src/main.rs");
        let addr =
            resolve_addr_in("src/main.rs", &PkgBuf::from("cmd/server"), root.path()).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/src/main.rs"));
    }

    #[test]
    fn unparseable_missing_path_surfaces_parse_error() {
        // Not an address and not a file on disk → the address parse error wins.
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_addr_in("data.txt", &PkgBuf::from("cmd/server"), dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("parse data.txt"), "{err:#}");
    }

    #[test]
    fn dot_slash_path_to_existing_file_uses_fs_target() {
        // `./somefile.txt` is not a valid address (a relative path ref must name
        // a target), so an existing file takes the fs file sugar.
        let root = root_with_file("cmd/server/somefile.txt");
        let addr =
            resolve_addr_in("./somefile.txt", &PkgBuf::from("cmd/server"), root.path()).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/somefile.txt"));
    }

    #[test]
    fn dot_slash_relative_target_with_explicit_name() {
        // The explicit `:name` form is a target address, parsed before any disk
        // check.
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_addr_in("./sub:thing", &PkgBuf::from("cmd/server"), dir.path()).unwrap();
        assert_eq!(addr, parse_addr("//cmd/server/sub:thing").unwrap());
    }

    #[test]
    fn plain_addr_is_parsed_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_addr_in("//lib:core", &PkgBuf::from("cmd/server"), dir.path()).unwrap();
        assert_eq!(addr, parse_addr("//lib:core").unwrap());
    }

    #[test]
    fn colon_relative_target_resolves_against_package() {
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_addr_in(":mytarget", &PkgBuf::from("cmd/server"), dir.path()).unwrap();
        assert_eq!(addr, parse_addr("//cmd/server:mytarget").unwrap());
    }
}
