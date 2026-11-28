use crate::tui::session::SessionSlot;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use enclose::enclose;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::tool::codegen::gitignore::stale_messages;
use crate::commands::tool::codegen::status::{self as codegen_status, ItemKind, StatusReport};
use crate::commands::utils::package_matcher_or_all;
use crate::engine::error::MultiError;
use crate::engine::query::skip_unresolvable;
use crate::engine::request_state::RequestState;
use crate::engine::{Discovery, Engine, Gaps, get_root, gitignore};
use crate::htaddr::Addr;
use crate::htmatcher::{MatchResult, Matcher};
use crate::htpkg::PkgBuf;
use crate::pluginbuildfile::ShadowedChecks;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
#[command(override_usage = "heph validate\n       heph validate <PACKAGE_MATCHER>")]
pub struct ValidateArgs {
    /// Package matcher (e.g. //pkg/...); omit to validate the whole workspace
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub matcher: Option<String>,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<ValidateView>,
}

/// `heph validate --json`: what the checks found. Printed whether or not they
/// passed; a problem still fails the command, as without `--json`. A target
/// that fails to resolve or link is a build failure, reported on stderr with
/// no document.
#[derive(Debug, Default, serde::Serialize, output::Record)]
pub struct ValidateView {
    /// One message per problem that fails validation; empty when it passed.
    pub problems: Vec<String>,
    /// Findings that do not fail it: generated files local to this checkout,
    /// and a check skipped because validation is scoped.
    pub warnings: Vec<String>,
}

impl ValidateView {
    /// The view of a check that ended with `checked`: its problems, flattened
    /// out of [`finish`]'s error, and the `warnings` it printed.
    fn new(checked: &anyhow::Result<()>, warnings: Vec<String>) -> Self {
        let problems = match checked {
            Ok(()) => Vec::new(),
            Err(e) => match e.downcast_ref::<MultiError>() {
                Some(MultiError(inner)) => inner.iter().map(|p| format!("{p:#}")).collect(),
                // One problem, under `finish`'s "validation failed".
                None => {
                    let under: Vec<String> = e.chain().skip(1).map(ToString::to_string).collect();
                    if under.is_empty() {
                        vec![format!("{e:#}")]
                    } else {
                        vec![under.join(": ")]
                    }
                }
            },
        };
        Self { problems, warnings }
    }
}

struct ValidateApp {
    validate: Validate,
    fail_fast: bool,
    out: output::JsonArgs<ValidateView>,
}

/// What validate checks, and against which engine.
struct Validate {
    engine: Arc<Engine>,
    /// Targets to validate — whole workspace or the user-scoped matcher.
    matcher: Matcher,
    /// True when the user passed a matcher; the gitignore check is then skipped.
    scoped: bool,
    root: PathBuf,
    /// The buildfile provider's record of shadowed check targets (I2).
    shadowed: ShadowedChecks,
}

#[async_trait]
impl App for ValidateApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new("Validating targets".to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new("Validating targets".to_string())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let ValidateApp {
            validate,
            fail_fast,
            out: json,
        } = self;
        let rs = validate
            .engine
            .new_state_with_events(fail_fast, ctx.event_sender());
        // One sink for the link and overlap walks: a candidate both skip counts
        // once. The `.gitignore` check stays complete — a short set would report
        // a correct file as out of date.
        let gaps = Gaps::new(crate::htquery::format(&validate.matcher));
        let discovery = Discovery::keep_going_unless(fail_fast, &gaps);

        if json.enabled() {
            // Stdout is the document's (I1): the warnings go into it.
            let warnings = std::sync::Mutex::new(Vec::new());
            let res = validate
                .check(&rs, discovery, &|line: &str| {
                    if let Ok(mut w) = warnings.lock() {
                        w.push(line.to_owned());
                    }
                })
                .await;
            // The problems are the document's, so they reach the body; a
            // cancellation stays an error for `finalize` to surface.
            let res = match res {
                Err(e) if crate::commands::errors::is_cancelled(&e) => Err(e),
                checked => Ok(checked),
            };
            let cancelled = || rs.ctoken().is_cancelled();
            return crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |checked| {
                let warnings = warnings.into_inner().unwrap_or_default();
                json.record(&ValidateView::new(&checked, warnings), &cancelled)?;
                checked
            })
            .await;
        }

        let out = BufferedStdout::new(&ctx);
        let res = validate
            .check(&rs, discovery, &|line: &str| out.println(line))
            .await;
        out.close().await;

        crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(())).await
    }
}

impl Validate {
    /// Every check, in order, on `rs`: the body of [`ValidateApp::run`]
    /// without the TUI. Warnings go to `println`; the problems come back as
    /// one error.
    async fn check(
        &self,
        rs: &Arc<RequestState>,
        discovery: Discovery,
        println: &(dyn Fn(&str) + Sync),
    ) -> anyhow::Result<()> {
        let Validate {
            engine,
            matcher,
            scoped,
            root,
            shadowed,
        } = self;
        let scoped = *scoped;

        // Overlap detection scopes to the user matcher when scoped, else uses the
        // codegen-tree selector (same one the gitignore enumeration uses).
        let overlap_matcher = if scoped {
            matcher.clone()
        } else {
            Matcher::TreeOutputTo(PkgBuf::from(""))
        };

        async {
            // Each check runs to completion before the next begins, and that is
            // a correctness requirement rather than a style choice.
            //
            // Each drives its own `Engine::query` walk, and a walk's
            // `MatchShrug` arm resolves candidates on a *speculative*
            // `RequestState` whose cycle check walks per-chain breadcrumbs
            // instead of the shared `DepDag`. That is sound only while one such
            // chain exists at a time — a guarantee `Engine::query` makes *per
            // walk* (its arm sits in the consumer of its own fan-out) and
            // therefore cannot make across walks. `Engine::query`'s own comment
            // names this command as the way the guarantee gets broken.
            //
            // Two chains at once are mutually invisible, and both consequences
            // reach a build definition: `mem_spec`/`mem_def` are shared and keyed
            // by addr, so whichever chain creates the cell decides which provider
            // resolved the addr — and `hashin` folds `def.driver`, so the race
            // picks the cache key. And two chains that resolve each other close a
            // cycle neither can see, which hangs where the serial path reported an
            // error. Both are reachable here: unscoped, checks 2 and 3 run the
            // *same* `TreeOutputTo("")` matcher, which shrugs at the addr *and*
            // the spec level, so both walks drive `get_spec` and `get_def`
            // speculatively over the whole workspace.
            //
            // Serialising costs much less than three times one walk: the three
            // share this request's memoizers, so the second and third walk hit
            // cells the first already filled. It is the *speculative chains* that
            // must not overlap, not the work they memoize.
            //
            // Every result is bound rather than `?`-propagated, so a failing
            // check never short-circuits the others and `finish` reports all of
            // them at once.

            // 1. Link every in-scope target: parse + resolve its runtime inputs.
            //    No execution — proves the graph is well-formed.
            let link_res: anyhow::Result<()> = async {
                let addrs: Vec<Addr> = Arc::clone(engine)
                    .query(rs.clone(), matcher, discovery.clone())
                    .try_collect()
                    .await?;
                let futs = addrs.iter().map(|addr| {
                    enclose!((engine, rs, addr) async move {
                        // A listed candidate that doesn't resolve standalone —
                        // e.g. go per-platform variants that only resolve as
                        // in-context deps — has no graph of its own to link. The
                        // helper rather than a `query_*` stream: the fan-out below
                        // reports *every* failure, and a stream stops at the
                        // first.
                        let res = Arc::clone(&engine).get_def(rs.clone(), &addr).await;
                        let Some(def) = skip_unresolvable(&addr, res)? else {
                            return Ok(());
                        };
                        Arc::clone(&engine)
                            .link(rs.clone(), Arc::clone(&def.target_def))
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    })
                });
                // Always aggregate: validate reports every broken target, not
                // just the first one to fail. This fan-out is genuinely
                // concurrent and stays so — it resolves real deps on `rs`, not
                // speculative candidates, so it starts no chain.
                crate::engine::fanout::join_all_failable(futs, false).await?;
                Ok(())
            }
            .await;

            // 2. Detect overlapping `codegen = copy` outputs.
            let overlap_res = Arc::clone(engine)
                .codegen_copy_overlaps(rs.clone(), &overlap_matcher, discovery.clone())
                .await;

            // 3. The codegen checks (whole-workspace runs only).
            let codegen = if scoped {
                CodegenFindings::default()
            } else {
                codegen_findings(engine, rs, root).await
            };
            let CodegenFindings {
                failures: gitignore_res,
                warnings: local_warnings,
            } = codegen;

            // 4. Every provider lists exactly the facts its targets resolve to,
            //    and lists only what it resolves. The walks trust a listed No
            //    and never check it; this does.
            let listed_facts_res = Arc::clone(engine)
                .listed_fact_mismatches(rs.clone(), matcher, discovery.clone())
                .await
                .map(|mismatches| mismatches.iter().map(ToString::to_string).collect());

            // 5. No target shadows a BUILD-file check target (I2). The provider
            //    records each as it lists a package, and check 1's walk listed
            //    every package in scope. Run and query never read the record.
            let shadowed_res = Ok(shadowed_check_problems(shadowed, matcher));

            let overlap_res = overlap_res.map(|overlaps| {
                overlaps
                    .iter()
                    .map(|o| {
                        format!(
                            "codegen=copy outputs overlap: `{}` ({}) and `{}` ({})",
                            o.a.shown,
                            o.a.addr.format(),
                            o.b.shown,
                            o.b.addr.format()
                        )
                    })
                    .collect::<Vec<String>>()
            });
            for w in &local_warnings {
                println(w);
            }

            finish(vec![
                link_res.map(|()| Vec::new()),
                overlap_res,
                gitignore_res,
                listed_facts_res,
                shadowed_res,
            ])?;

            // Success prints nothing; only the scoped-skip warning is emitted.
            if scoped {
                println("warning: skipped .gitignore freshness check (validation is scoped)");
            }
            Ok(())
        }
        .await
    }
}

/// What `validate`'s codegen checks found.
#[derive(Debug)]
pub struct CodegenFindings {
    /// What fails validation: one message per stale-section reason (a line
    /// no target emits, a missing one, the old marker, a malformed section).
    pub failures: anyhow::Result<Vec<String>>,
    /// Findings local to this checkout, printed as warnings.
    pub warnings: Vec<String>,
}

impl Default for CodegenFindings {
    fn default() -> Self {
        Self {
            failures: Ok(Vec::new()),
            warnings: Vec::new(),
        }
    }
}

/// `validate`'s codegen checks. Only a stale section fails: it is what the
/// commit decides, and a line whose target is missing is stale because no def
/// emits it. Everything else `codegen status` lists is local to this checkout
/// and only warned about; `validate` reads and settles nothing.
pub async fn codegen_findings(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    root: &Path,
) -> CodegenFindings {
    let failures: anyhow::Result<Vec<String>> = async {
        let entries = Arc::clone(engine)
            .codegen_copy_gitignore_patterns(rs.clone(), &Matcher::TreeOutputTo(PkgBuf::from("")))
            .await?;
        let existing = crate::commands::tool::codegen::read_gitignore(root)?;
        // No codegen target and no section to keep fresh: a workspace that
        // never used codegen passes without a `.gitignore` (CLI-17). A section
        // left behind by targets that are gone is still checked.
        if entries.is_empty() && !gitignore::has_markers(&existing) {
            return Ok(Vec::new());
        }
        Ok(stale_messages(
            &gitignore::diff_section(&existing, &entries).reasons,
        ))
    }
    .await;
    // Serial with the check above, for the reason `run` gives.
    let opts = codegen_status::Options::VALIDATE;
    let warnings = match codegen_status::status(engine, rs, opts).await {
        Ok(report) => local_findings_warnings(&report),
        Err(e) => vec![format!(
            "warning: could not list codegen findings: {e:#}; run heph tool codegen status"
        )],
    };
    CodegenFindings { failures, warnings }
}

/// The warnings `validate` prints for findings local to this checkout: every
/// item `codegen status` lists except a stale section, which fails instead,
/// and a target or package that did not resolve, which the link check
/// already fails on (CLI-17/STAGE-21: each cause once, and not as a codegen
/// finding). One line per item, then one pointing at `heph tool codegen
/// status`.
fn local_findings_warnings(report: &StatusReport) -> Vec<String> {
    let mut out: Vec<String> = report
        .items
        .iter()
        .filter(|i| !matches!(i.kind, ItemKind::SectionStale | ItemKind::Unresolved))
        .map(|i| {
            let head = i.path.as_deref().or(i.package.as_deref()).unwrap_or("");
            format!(
                "warning: codegen: {head}: {} ({})",
                i.detail,
                i.kind.as_str()
            )
        })
        .collect();
    if !out.is_empty() {
        out.push(format!(
            "warning: {} generated file finding{} local to this checkout; run heph tool codegen status",
            out.len(),
            if out.len() == 1 { "" } else { "s" }
        ));
    }
    out
}

/// One problem per check target a target in `matcher`'s scope shadows (I2),
/// in package order. Empty without the buildfile plugin, which is the only
/// writer of `shadowed`.
fn shadowed_check_problems(shadowed: &ShadowedChecks, matcher: &Matcher) -> Vec<String> {
    shadowed
        .all()
        .iter()
        .filter(|s| !matches!(matcher.matches_addr(&s.addr), MatchResult::MatchNo))
        .map(ToString::to_string)
        .collect()
}

/// Fold the outcome of every validate check into a single result. Each check
/// contributes either a list of human-readable problem strings (`Ok`) or a hard
/// error (`Err`); a check's `Err` that is itself a [`MultiError`] is flattened
/// so its inner errors surface individually. The point is exhaustiveness: nothing
/// short-circuits, so the user sees *all* the problems, not just the first —
/// each once ([`dedupe_by_cause`]).
fn finish(checks: Vec<anyhow::Result<Vec<String>>>) -> anyhow::Result<()> {
    let mut errs: Vec<anyhow::Error> = Vec::new();
    for check in checks {
        match check {
            Ok(problems) => errs.extend(problems.into_iter().map(|p| anyhow::anyhow!(p))),
            Err(e) => match e.downcast::<MultiError>() {
                Ok(MultiError(inner)) => errs.extend(inner),
                Err(e) => errs.push(e),
            },
        }
    }
    let mut errs = dedupe_by_cause(errs);
    let combined = match errs.len() {
        0 => return Ok(()),
        1 => errs.pop().expect("len == 1"),
        _ => MultiError(errs).into(),
    };
    Err(combined).context("validation failed")
}

/// One error per (root, cause) (error-reporting D8, CLI-17/STAGE-21). The
/// link walk reaches a broken target from each target that depends on it,
/// and the other checks walk the workspace again, so one cause arrives once
/// per path. The root is the culprit the engine blames
/// ([`crate::engine::culprit_on_chain`]) and the cause its innermost frame;
/// an error that names no culprit is keyed by its whole text. Each cause
/// keeps the place of its first arrival and the shortest chain it arrived
/// with, which is the culprit's own error when the walk reached it.
fn dedupe_by_cause(errs: Vec<anyhow::Error>) -> Vec<anyhow::Error> {
    type Key = (Option<hplugin::error::Culprit>, String);
    let mut out: Vec<(anyhow::Error, usize)> = Vec::with_capacity(errs.len());
    let mut seen: std::collections::HashMap<Key, usize> = std::collections::HashMap::new();
    for e in errs {
        let frames = crate::hmemoizer::frames(&e);
        let depth = frames.len();
        let key = match crate::engine::culprit_on_chain(&e) {
            Some(culprit) => {
                let cause = frames.last().map(|f| f.error().to_string());
                (Some(culprit), cause.unwrap_or_default())
            }
            None => (None, format!("{e:#}")),
        };
        drop(frames);
        match seen.get(&key).and_then(|&i| out.get_mut(i)) {
            Some(kept) if depth < kept.1 => *kept = (e, depth),
            Some(_) => {}
            None => {
                seen.insert(key, out.len());
                out.push((e, depth));
            }
        }
    }
    out.into_iter().map(|(e, _)| e).collect()
}

/// Every check `heph validate` runs on the whole workspace at `root`, with
/// the engine `heph validate` builds there: the problems that fail it and the
/// warnings it printed, as `--json` reports them. For tests.
pub async fn validate_workspace_at(root: &Path) -> anyhow::Result<ValidateView> {
    let (engine, _shutdown, shadowed) =
        bootstrap::new_validate_engine_at(root, &GlobalOptions::default(), None)?;
    let validate = Validate {
        engine: Arc::clone(&engine),
        matcher: Matcher::PackagePrefix(PkgBuf::from("")),
        scoped: false,
        root: root.to_path_buf(),
        shadowed,
    };
    let gaps = Gaps::new(crate::htquery::format(&validate.matcher));
    let warnings = std::sync::Mutex::new(Vec::new());
    let checked = validate
        .check(
            &engine.new_state(),
            Discovery::keep_going_unless(false, &gaps),
            &|line: &str| {
                if let Ok(mut w) = warnings.lock() {
                    w.push(line.to_owned());
                }
            },
        )
        .await;
    let warnings = warnings.into_inner().unwrap_or_default();
    Ok(ValidateView::new(&checked, warnings))
}

pub fn execute(
    args: &ValidateArgs,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: ValidateArgs,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let root = get_root()?;
    let scoped = args.matcher.is_some();
    let matcher = package_matcher_or_all(args.matcher.as_deref())?;
    let (engine, shutdown, shadowed) = bootstrap::new_validate_engine(&global, session)?;
    let app = ValidateApp {
        validate: Validate {
            engine,
            matcher,
            scoped,
            root,
            shadowed,
        },
        fail_fast: global.fail_fast,
        out: args.out.clone(),
    };
    // Under the output flags stdout carries the record alone (I1).
    let interactive = tui::should_use_tui(global.no_tui) && !args.out.enabled();
    tui::run_app_with(
        app,
        sink,
        session,
        interactive,
        args.dashboard.dashboard,
        shutdown,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{ItemKind, StatusReport, finish, local_findings_warnings, stale_messages};
    use crate::commands::tool::codegen::status::StatusItem;
    use crate::engine::error::MultiError;
    use crate::engine::gitignore::StaleReason;

    #[test]
    fn all_checks_ok_is_ok() {
        assert!(finish(vec![Ok(vec![]), Ok(vec![]), Ok(vec![])]).is_ok());
    }

    #[test]
    fn reports_every_problem_not_just_the_first() {
        // A link failure, an overlap, and a stale gitignore — all three must
        // appear in the rendered error, proving nothing short-circuited.
        let err = finish(vec![
            Err(anyhow::anyhow!("link broke for //pkg:a")),
            Ok(vec!["codegen overlap on `gen.rs`".to_string()]),
            Ok(stale_messages(&[StaleReason::Lines])),
        ])
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("link broke for //pkg:a"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("codegen overlap on `gen.rs`"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("is out of date; run heph tool codegen gitignore"),
            "got: {rendered}"
        );
        assert!(!rendered.contains("gen-gitignore"), "got: {rendered}");
    }

    fn item(kind: ItemKind, path: &str) -> StatusItem {
        StatusItem {
            kind,
            path: Some(path.to_string()),
            addr: Some("//p:gen".to_string()),
            output: None,
            fix: "heph tool codegen gitignore".to_string(),
            detail: format!("detail of {path}"),
            package: None,
            error: None,
        }
    }

    #[test]
    fn local_findings_skip_the_stale_section_and_point_at_status() {
        // An unresolved target is the link check's to report (CLI-17).
        let report = StatusReport {
            check: false,
            items: vec![
                item(ItemKind::SectionStale, ".gitignore"),
                item(ItemKind::NoLongerGenerated, "p/a.go"),
                item(ItemKind::Unresolved, "//r:t"),
                item(ItemKind::ForeignLine, "/q/b.go"),
            ],
            hint: None,
        };
        let w = local_findings_warnings(&report);
        assert_eq!(w.len(), 3, "two items and the pointer: {w:?}");
        assert!(w[0].contains("p/a.go") && w[0].contains("no_longer_generated"));
        assert!(w[1].contains("/q/b.go") && w[1].contains("foreign_line"));
        assert!(w[2].contains("2 generated file findings"), "{w:?}");
        assert!(w[2].ends_with("run heph tool codegen status"), "{w:?}");
        assert!(w.iter().all(|l| l.starts_with("warning: ")));
        assert!(!w.iter().any(|l| l.contains(".gitignore")), "{w:?}");

        let clean = StatusReport {
            items: vec![item(ItemKind::SectionStale, ".gitignore")],
            ..StatusReport::default()
        };
        assert!(local_findings_warnings(&clean).is_empty());
    }

    #[test]
    fn flattens_nested_multierror_from_a_check() {
        // The link fanout returns a MultiError when several targets fail; its
        // inner errors must be hoisted into the top-level list, not nested.
        let link_err = MultiError(vec![
            anyhow::anyhow!("target a failed"),
            anyhow::anyhow!("target b failed"),
        ]);
        let err = finish(vec![
            Err(link_err.into()),
            Ok(vec!["overlap c".to_string()]),
            Ok(vec![]),
        ])
        .unwrap_err();
        let multi = err
            .downcast_ref::<MultiError>()
            .expect("expected a flattened MultiError");
        assert_eq!(multi.0.len(), 3, "two link errors + one overlap");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("target a failed"), "got: {rendered}");
        assert!(rendered.contains("target b failed"), "got: {rendered}");
        assert!(rendered.contains("overlap c"), "got: {rendered}");
    }

    #[test]
    fn json_view_lists_every_problem_without_the_envelope() -> anyhow::Result<()> {
        let several = finish(vec![
            Err(MultiError(vec![anyhow::anyhow!("target a failed")]).into()),
            Ok(vec!["overlap c".to_string()]),
        ]);
        let view = super::ValidateView::new(&several, vec!["warning: w".to_string()]);
        assert_eq!(view.problems, ["target a failed", "overlap c"]);
        assert_eq!(view.warnings, ["warning: w"]);

        let one = finish(vec![Err(anyhow::anyhow!("inner").context("outer"))]);
        let view = super::ValidateView::new(&one, Vec::new());
        assert_eq!(
            view.problems,
            ["outer: inner"],
            "not \"validation failed: …\""
        );

        let ok = super::ValidateView::new(&Ok(()), Vec::new());
        assert!(ok.problems.is_empty());
        output::check_keys(&view)
    }

    #[test]
    fn single_problem_is_returned_unwrapped() {
        // One problem stays a plain error (no "N errors:" envelope).
        let err = finish(vec![
            Ok(vec!["lonely overlap".to_string()]),
            Ok(vec![]),
            Ok(vec![]),
        ])
        .unwrap_err();
        assert!(err.downcast_ref::<MultiError>().is_none());
        assert!(format!("{err:#}").contains("lonely overlap"));
    }

    // ---- shadowed check targets (I2) ---------------------------------------

    use super::shadowed_check_problems;
    use crate::engine::{Config, Discovery, Engine, PluginParts};
    use crate::htmatcher::Matcher;
    use crate::htpkg::PkgBuf;
    use crate::pluginbuildfile::ShadowedChecks;
    use futures::TryStreamExt;
    use hcore::htvalue::Value;
    use hcore::htvalue::signature::{FnSignature, Param, ParamType};
    use hplugin::function::{
        DeclaredTarget, FnArgs, FnCallContext, FnOutcome, PluginFn, PluginFnDef,
    };
    use std::sync::Arc;

    /// `heph.codegen.rule(name = …)`: declares an `exec` target named `name`.
    struct RuleFn;

    #[async_trait::async_trait]
    impl PluginFn for RuleFn {
        async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
            let Some(Value::String(name)) = args.named.get("name") else {
                anyhow::bail!("rule: `name` must be a string");
            };
            let mut outcome = FnOutcome::from(Value::String(format!("//{}:{name}", ctx.pkg)));
            outcome.declare_target(DeclaredTarget {
                name: name.clone(),
                driver: "exec".to_string(),
                ..Default::default()
            });
            Ok(outcome)
        }
    }

    /// The files under `root`, an engine over them with the buildfile plugin
    /// built by the binary's own factory and a declaring `heph.codegen.rule`,
    /// and the shadow record that factory was given.
    fn shadow_ws(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Engine>, ShadowedChecks) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, content) in files {
            let abs = dir.path().join(path);
            std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
            std::fs::write(&abs, content).expect("write");
        }
        let mut e = Engine::new(Config::for_tests(dir.path().to_path_buf())).expect("engine");
        let shadowed = ShadowedChecks::default();
        crate::commands::bootstrap::register_builtin_factories_with(
            &mut e,
            dir.path(),
            shadowed.clone(),
        )
        .expect("factories");
        e.register_plugin("codegen", |_| {
            Ok(PluginParts::default().with_functions(vec![PluginFnDef {
                name: "rule".to_string(),
                signature: FnSignature {
                    positional: vec![],
                    named: vec![Param::required("name", ParamType::String)],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: "Declare an exec target.".to_string(),
                func: Arc::new(RuleFn),
            }]))
        })
        .expect("codegen plugin");
        e.apply_builtin("buildfile", &Default::default())
            .expect("buildfile");
        (dir, Arc::new(e), shadowed)
    }

    /// Run validate's link walk over `matcher` (which lists every package in
    /// scope), then its shadow check.
    async fn shadow_problems(
        engine: &Arc<Engine>,
        shadowed: &ShadowedChecks,
        matcher: &Matcher,
    ) -> Vec<String> {
        let _: Vec<_> = Arc::clone(engine)
            .query(engine.new_state(), matcher, Discovery::Complete)
            .try_collect()
            .await
            .expect("query");
        shadowed_check_problems(shadowed, matcher)
    }

    const FIX: &str = "rename the target, or remove it to get the check back";

    /// The wiring, end to end in-process: the engine `heph validate` builds
    /// hands validate the same record the buildfile provider writes to, and
    /// validate's own check sequence turns a shadow into its failure.
    #[tokio::test]
    async fn validate_engine_reports_shadow() {
        use crate::engine::gitignore::{BEGIN_MARKER, END_MARKER};
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, content) in [
            (
                ".hephconfig",
                "plugins:\n  - builtin: buildfile\ntelemetry:\n  enabled: false\n",
            ),
            (
                "pkg/BUILD",
                "target(name = \"build-format\", driver = \"textfile\", text = \"mine\")\n",
            ),
            (
                "clean/BUILD",
                "target(name = \"t\", driver = \"textfile\", text = \"x\")\n",
            ),
        ] {
            let abs = dir.path().join(path);
            std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
            std::fs::write(&abs, content).expect("write");
        }
        // Fresh, so the shadow is the only problem.
        std::fs::write(
            dir.path().join(".gitignore"),
            format!("{BEGIN_MARKER}\n{END_MARKER}\n"),
        )
        .expect("write .gitignore");
        let (engine, _shutdown, shadowed) = crate::commands::bootstrap::new_validate_engine_at(
            dir.path(),
            &crate::commands::GlobalOptions::default(),
            None,
        )
        .expect("validate engine");
        let validate = super::Validate {
            engine: Arc::clone(&engine),
            matcher: Matcher::PackagePrefix(PkgBuf::from("")),
            scoped: false,
            root: dir.path().to_path_buf(),
            shadowed,
        };
        let gaps = crate::engine::Gaps::new("//...".to_string());
        let err = validate
            .check(
                &engine.new_state(),
                Discovery::keep_going_unless(false, &gaps),
                &|_| {},
            )
            .await
            .expect_err("a shadowed check fails validate");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "validation failed: //pkg:build-format, declared at pkg/BUILD:1, shadows the \
                 BUILD-file check `build-format`: {FIX}"
            )
        );
    }

    #[tokio::test]
    async fn shadowed_check_target_fails() {
        let (_dir, engine, shadowed) = shadow_ws(&[
            (
                "pkg/BUILD",
                "target(name = \"t\", driver = \"exec\")\n\
                 target(name = \"build-lint-check\", driver = \"exec\")\n",
            ),
            (
                "other/BUILD",
                "target(name = \"build-format\", driver = \"exec\")\n",
            ),
            ("clean/BUILD", "target(name = \"t\", driver = \"exec\")\n"),
        ]);
        let all = Matcher::PackagePrefix(PkgBuf::from(""));
        let problems = shadow_problems(&engine, &shadowed, &all).await;
        assert_eq!(
            problems,
            [
                format!(
                    "//other:build-format, declared at other/BUILD:1, shadows the BUILD-file \
                     check `build-format`: {FIX}"
                ),
                format!(
                    "//pkg:build-lint-check, declared at pkg/BUILD:2, shadows the BUILD-file \
                     check `build-lint-check`: {FIX}"
                ),
            ]
        );
        // validate fails on them, naming each.
        let err = super::finish(vec![Ok(problems)]).expect_err("validate fails");
        let msg = format!("{err:#}");
        assert!(msg.contains("//pkg:build-lint-check"), "{msg}");
        assert!(msg.contains("//other:build-format"), "{msg}");

        // A scoped validate reports only its own scope.
        let scoped = Matcher::PackagePrefix(PkgBuf::from("pkg"));
        assert_eq!(
            shadow_problems(&engine, &shadowed, &scoped).await.len(),
            1,
            "only //pkg's conflict is in scope"
        );
    }

    #[tokio::test]
    async fn shadow_from_function_and_loaded_file() {
        let (_dir, engine, shadowed) = shadow_ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"mk\")\n\
                 heph.codegen.rule(name = \"build-format\")\n\
                 mk()\n",
            ),
            (
                "lib/defs.star",
                "def mk():\n    target(name = \"build-format-check\", driver = \"exec\")\n",
            ),
        ]);
        let all = Matcher::PackagePrefix(PkgBuf::from(""));
        assert_eq!(
            shadow_problems(&engine, &shadowed, &all).await,
            [
                format!(
                    "//pkg:build-format, declared at pkg/BUILD:2, shadows the BUILD-file \
                     check `build-format`: {FIX}"
                ),
                format!(
                    "//pkg:build-format-check, declared at lib/defs.star:2 (called from \
                     pkg/BUILD:3), shadows the BUILD-file check `build-format-check`: {FIX}"
                ),
            ]
        );
    }
}
