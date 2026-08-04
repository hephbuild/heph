use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::{CleanStats, Engine, get_cwp};
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::tui::{self, App, AppContext, LogSink, human_bytes};
use crate::{htaddr, htpkg, htquery};

#[derive(clap::Args, Clone)]
#[command(override_usage = "heph clean [<PACKAGE_MATCHER>|<TARGET_ADDRESS>]")]
pub struct CleanArgs {
    /// Package matcher (`//pkg`, `//pkg/...`, `./...`) or target address
    /// (`//pkg:name`, `:name`). Defaults to `//...` — the whole local cache.
    #[arg(value_name = "PACKAGE_MATCHER/TARGET_ADDRESS", add = ArgValueCompleter::new(complete_target_addr))]
    pub target: Option<String>,
}

/// Resolve the positional argument into the addr-only matcher `Engine::clean`
/// takes. Absent, it is `//...`: the whole local cache.
///
/// The two forms are told apart by the `:` an address requires — see
/// `parse_addr_with_base`, where every accepted form (`//pkg:name`, `:name`,
/// `./sub:name`) carries one and a bare path deliberately does not. Picking the
/// parser up front rather than trying both means a malformed input gets the
/// diagnostic for the form the user was evidently writing, instead of whichever
/// of two parsers happened to fail last.
fn resolve_selection(target: Option<&str>, cwp: &PkgBuf) -> anyhow::Result<Matcher> {
    let Some(target) = target else {
        // `//...` — every package, so every cached addr.
        return Ok(Matcher::PackagePrefix(PkgBuf::from("")));
    };
    if target.contains(':') {
        let addr = htaddr::parse_addr_with_base(target, cwp)
            .with_context(|| format!("parse {target} as a target address"))?;
        return Ok(Matcher::Addr(addr));
    }
    htpkg::parse(target, cwp).with_context(|| {
        format!("parse {target} as a package matcher (a target address must name a target, e.g. {target}:name)")
    })
}

struct CleanApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
}

impl CleanApp {
    /// Label for both views. Names what is being cleaned, so the TUI header and
    /// the CI summary line say which selection the counts belong to.
    fn label(&self) -> String {
        format!("Cleaning {}", htquery::format(&self.matcher))
    }
}

#[async_trait]
impl App for CleanApp {
    type Output = ();
    type TuiView = tui::TuiProgressView;
    type CiView = tui::GcCiView;

    fn tui_view(&self) -> Self::TuiView {
        tui::TuiProgressView::with_header(Box::new(tui::GcHeader::new(self.label())))
    }

    fn ci_view(&self) -> Self::CiView {
        tui::GcCiView::new(self.label())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        let res = self.engine.clone().clean(rs.clone(), &self.matcher).await;
        let selection = htquery::format(&self.matcher);

        crate::commands::errors::finalize!(ctx, rs, res, stats => {
            print_summary(&stats, &selection);
            Ok(())
        })
    }
}

/// The one line the command exists to print.
///
/// A selection that matched no cached target is **not** an error — `clean` is
/// idempotent and its postcondition ("nothing cached for this selection") already
/// holds — but it is the outcome most likely to be a typo, so it says so
/// explicitly instead of reporting a `0`-shaped success that reads identically to
/// having freed nothing.
fn print_summary(stats: &CleanStats, selection: &str) {
    if stats.targets_matched == 0 {
        println!("Nothing to clean: no cached entries match {selection}");
        return;
    }
    println!(
        "Cleaned {} target(s) · {} revision(s) · {} freed",
        stats.targets_cleaned,
        stats.revisions_removed,
        human_bytes(stats.bytes_removed),
    );
    if stats.errored > 0 {
        // Counted, not fatal: the run kept going past a target it could not
        // delete, and a summary that hid that would claim a cleaner cache than
        // there is.
        println!(
            "{} target(s) could not be cleaned; see the logs above",
            stats.errored
        );
    }
}

pub fn execute(args: &CleanArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(
    args: CleanArgs,
    sink: LogSink,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let matcher = resolve_selection(args.target.as_deref(), &cwp)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = CleanApp {
        engine,
        matcher,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_argument_selects_the_whole_cache() {
        let m = resolve_selection(None, &PkgBuf::from("some/pkg")).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("")));
    }

    #[test]
    fn absolute_address_selects_exactly_that_target() {
        let m = resolve_selection(Some("//foo/bar:baz"), &PkgBuf::from("")).unwrap();
        match m {
            Matcher::Addr(a) => {
                assert_eq!(a.package.as_str(), "foo/bar");
                assert_eq!(a.name, "baz");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn colon_name_resolves_against_the_current_package() {
        let m = resolve_selection(Some(":build"), &PkgBuf::from("cmd/server")).unwrap();
        match m {
            Matcher::Addr(a) => {
                assert_eq!(a.package.as_str(), "cmd/server");
                assert_eq!(a.name, "build");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn an_addr_with_args_is_still_an_address() {
        // Variant args carry no `:`, but the target reference does — the form is
        // decided by the address's colon, not by what follows it.
        let m = resolve_selection(Some("//go/std:build@variant=race"), &PkgBuf::from("")).unwrap();
        match m {
            Matcher::Addr(a) => {
                assert_eq!(a.name, "build");
                assert_eq!(a.args.get("variant").map(String::as_str), Some("race"));
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn package_matcher_forms_are_packages_not_addresses() {
        let base = PkgBuf::from("cmd/server");
        assert_eq!(
            resolve_selection(Some("//foo/bar"), &base).unwrap(),
            Matcher::Package(PkgBuf::from("foo/bar"))
        );
        assert_eq!(
            resolve_selection(Some("//foo/..."), &base).unwrap(),
            Matcher::PackagePrefix(PkgBuf::from("foo"))
        );
        assert_eq!(
            resolve_selection(Some("//..."), &base).unwrap(),
            Matcher::PackagePrefix(PkgBuf::from(""))
        );
        assert_eq!(
            resolve_selection(Some("./..."), &base).unwrap(),
            Matcher::PackagePrefix(PkgBuf::from("cmd/server"))
        );
    }

    #[test]
    fn a_bare_word_is_reported_as_a_package_matcher_failure() {
        // No `:`, so it is read as a package reference — and the error points at
        // both forms rather than leaving the user to guess which parser spoke.
        let err = resolve_selection(Some("foo"), &PkgBuf::from("")).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("package matcher"), "{chain}");
        assert!(chain.contains("foo:name"), "{chain}");
    }

    #[test]
    fn a_malformed_address_gets_the_address_diagnostic() {
        let err = resolve_selection(Some("nope:thing"), &PkgBuf::from("")).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("target address"), "{chain}");
    }
}
