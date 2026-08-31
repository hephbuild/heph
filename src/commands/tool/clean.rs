use std::sync::Arc;

use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::run::QUERY_LANG_HELP;
use crate::commands::utils::resolve_matcher;
use crate::engine::{CleanStats, Engine, get_cwp};
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::htquery;
use crate::tui::{self, App, AppContext, LogSink, human_bytes};

#[derive(clap::Args, Clone)]
#[command(
    arg_required_else_help = true,
    override_usage = "heph tool clean <TARGET_ADDRESS>\n       heph tool clean <LABEL> <PACKAGE_MATCHER>\n       heph tool clean -e <EXPR>",
    after_long_help = QUERY_LANG_HELP
)]
pub struct CleanArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL", add = ArgValueCompleter::new(complete_target_addr))]
    pub arg1: Option<String>,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Select targets with a query expression, e.g. -e '//pkg/... && !//vendor/...'.
    /// Supports &&, ||, !, parentheses, and the label()/tree_output() functions.
    /// Mutually exclusive with the positional TARGET arguments.
    #[arg(
        short = 'e',
        long = "expr",
        value_name = "EXPR",
        conflicts_with = "arg1"
    )]
    pub expr: Option<String>,
    /// Also delete persistent scratch caches. With an ADDRESS, only that
    /// declaration's cache; alone, every one in the workspace.
    ///
    /// A flag rather than its own command because `clean` already means "delete
    /// what I point at", and a scratch is one more thing you can point at.
    /// `heph tool scratch rm` is the same operation for someone who is only
    /// thinking about scratch.
    #[arg(long)]
    pub scratch: bool,
}

/// Resolve the selection — `run`'s and `query`'s, with **no default**.
///
/// A bare `heph tool clean` deliberately does not mean `//...`: this command
/// deletes, and a whole-cache wipe must not be the cheapest thing to type, where
/// one slipped Enter costs every warm entry on the machine. Clearing everything
/// is still one command, but it has to be asked for — `heph tool clean all
/// //...`. `arg_required_else_help` catches the bare invocation before it
/// reaches here; this covers the rest (e.g. flags but no selection), where clap
/// sees arguments and does not fire.
///
/// `allow_all` is on, as it is for `query` and not for `run`. `all //some/dir` is
/// how you name a package without naming a label, and it is the form that keeps
/// `clean` off the graph entirely (`run` has no such stake — it must resolve
/// whatever it selects, so the shorthand buys it nothing).
fn selection(args: &CleanArgs, cwp: &PkgBuf) -> anyhow::Result<Matcher> {
    resolve_matcher(&args.expr, &args.arg1, &args.arg2, cwp, true)
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
    if stats.targets_cleaned == 0 && stats.errored == 0 {
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

    // `--scratch` alone means "clear the scratch store", with no target
    // selection to make — so it must not be forced through `selection`, which
    // deliberately refuses an empty one rather than wiping the target cache.
    if args.scratch && args.arg1.is_none() && args.expr.is_none() {
        let (engine, _shutdown) = bootstrap::new_engine(&global)?;
        let (n, freed) = engine.scratch_remove(None)?;
        println!("Removed {n} scratch cache(s), freed {freed} bytes.");
        return Ok(());
    }

    let matcher = selection(&args, &cwp)?;
    let (engine, shutdown) = bootstrap::new_engine(&global)?;
    if args.scratch
        && let Some(addr) = args.arg1.as_deref()
    {
        let (n, freed) = engine.scratch_remove(Some(addr))?;
        println!("Removed {n} scratch cache(s), freed {freed} bytes.");
    }
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
    use clap::Subcommand as _;

    /// Global options and a subcommand's own args live in one clap command, so
    /// two flags sharing an *id* is a runtime panic on access — not a build
    /// error, and not something `--help` surfaces.
    ///
    /// This bit: `clean` has `--scratch` (a bool) and the globals once had
    /// `--scratch` (a mode), both under the id `scratch`. Every `heph tool clean`
    /// died with "Mismatch between definition and access of `scratch`". Nothing
    /// caught it, because the only CLI test harness flattened the globals with no
    /// subcommand attached — the one arrangement where the two never meet.
    ///
    /// So this harness is the point of the test: globals *and* a subcommand,
    /// parsed together, with both values read.
    #[test]
    fn clean_and_the_globals_do_not_share_an_argument_id() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            global: crate::commands::global::GlobalOptions,
            #[command(subcommand)]
            cmd: TestCmd,
        }

        #[derive(clap::Subcommand)]
        enum TestCmd {
            Clean(CleanArgs),
        }

        let cli = TestCli::try_parse_from(["heph", "clean", "--scratch", "--no-scratch"])
            .expect("both flags must parse together");
        // Reading both is what trips a shared id; parsing alone would not.
        assert!(cli.global.no_scratch, "the global flag must be readable");
        let TestCmd::Clean(clean) = cli.cmd;
        assert!(clean.scratch, "clean's own flag must be readable");
    }

    /// The positional/expr forms, as clap would fill them. The `<LABEL>
    /// <PACKAGE_MATCHER>` pair is deliberately absent: `matcher_from_args`
    /// resolves that one against the *process* cwd via `engine::get_cwp()`, so it
    /// needs a real workspace and is covered by the e2e suite instead.
    fn args(arg1: Option<&str>, expr: Option<&str>) -> CleanArgs {
        CleanArgs {
            arg1: arg1.map(str::to_string),
            arg2: None,
            expr: expr.map(str::to_string),
            scratch: false,
        }
    }

    /// Parse a full `heph <args>` command line the way `main` does.
    fn parse(argv: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        crate::commands::Commands::augment_subcommands(clap::Command::new("heph"))
            .try_get_matches_from(argv)
    }

    #[test]
    fn a_bare_clean_is_refused_rather_than_wiping_the_cache() {
        // There is no whole-cache default. This command deletes, and the wipe
        // must not be the cheapest thing to type — clap sends the bare
        // invocation to the help text instead.
        let err = parse(&["heph", "tool", "clean"]).expect_err("a bare clean must not run");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn flags_without_a_selection_are_refused_too() {
        // `arg_required_else_help` only fires on a *completely* empty invocation,
        // so the flags-only case is the one that reaches `selection` — and it has
        // to fail there rather than fall back to a default.
        let err = selection(&args(None, None), &PkgBuf::from("some/pkg")).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("missing TARGET_ADDRESS/LABEL"), "{chain}");
    }

    #[test]
    fn the_whole_cache_is_still_reachable_explicitly() {
        // Refusing the bare form must remove the accident, not the capability.
        // Asserted through `-e` because the equivalent `all //...` resolves its
        // package against the *process* cwd (see `args`); e2e covers that one.
        let m = selection(&args(None, Some("//...")), &PkgBuf::from("")).unwrap();
        assert_eq!(m, Matcher::PackagePrefix(PkgBuf::from("")));
    }

    #[test]
    fn absolute_address_selects_exactly_that_target() {
        let m = selection(&args(Some("//foo/bar:baz"), None), &PkgBuf::from("")).unwrap();
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
        let m = selection(&args(Some(":build"), None), &PkgBuf::from("cmd/server")).unwrap();
        match m {
            Matcher::Addr(a) => {
                assert_eq!(a.package.as_str(), "cmd/server");
                assert_eq!(a.name, "build");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn a_variant_is_part_of_the_address() {
        let m = selection(
            &args(Some("//go/std:build@variant=race"), None),
            &PkgBuf::from(""),
        )
        .unwrap();
        match m {
            Matcher::Addr(a) => {
                assert_eq!(a.name, "build");
                assert_eq!(a.args.get("variant").map(String::as_str), Some("race"));
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn an_expression_selects_the_same_way_it_does_for_run() {
        let m = selection(
            &args(None, Some("//foo/... && !//foo/vendor/...")),
            &PkgBuf::from(""),
        )
        .unwrap();
        match m {
            Matcher::And(terms) => {
                assert_eq!(terms.len(), 2);
                assert!(matches!(terms[0], Matcher::PackagePrefix(_)));
                assert!(matches!(terms[1], Matcher::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn a_label_expression_is_accepted_and_left_for_the_engine_to_resolve() {
        // Not rejected at the CLI: `Engine::clean` answers this one from the
        // graph. All the CLI owes is the parse.
        let m = selection(&args(None, Some("label(test)")), &PkgBuf::from("")).unwrap();
        assert!(matches!(m, Matcher::Label(_)), "got {m:?}");
    }

    #[test]
    fn positional_and_expr_are_mutually_exclusive() {
        // Enforced by clap, the same `conflicts_with` `run` and `query` use, so
        // the two selections can never both be live.
        let err =
            parse(&["heph", "tool", "clean", "//a:b", "-e", "//c/..."]).expect_err("must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn a_bare_word_is_not_an_address() {
        // Same diagnostic `run` gives: a relative reference has to be
        // unmistakable.
        let err = selection(&args(Some("foo"), None), &PkgBuf::from("")).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("relative references must start"), "{chain}");
    }
}
