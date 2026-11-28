use crate::tui::session::SessionSlot;
use std::sync::Arc;

use clap_complete::engine::ArgValueCompleter;
use futures::{Stream, StreamExt, TryStreamExt};

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command_out;
use crate::commands::run::QUERY_LANG_HELP;
use crate::commands::utils::resolve_matcher;
use crate::engine::driver::TargetAddr;
use crate::engine::request_state::RequestState;
use crate::engine::{Discovery, Engine, Gaps, get_cwp};
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::tui::{BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
#[command(
    override_usage = "heph query <TARGET_ADDRESS>\n       heph query <LABEL> <PACKAGE_MATCHER>\n       heph query -e <EXPR>",
    after_long_help = QUERY_LANG_HELP
)]
pub struct Args {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL", add = ArgValueCompleter::new(complete_target_addr))]
    pub arg1: Option<String>,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Select targets with a query expression, e.g. -e '//pkg/... && !//vendor/...'.
    /// Supports &&, ||, !, parentheses, and the label()/driver()/tree_output() functions.
    /// Mutually exclusive with the positional TARGET arguments.
    #[arg(
        short = 'e',
        long = "expr",
        value_name = "EXPR",
        conflicts_with = "arg1"
    )]
    pub expr: Option<String>,
    /// Print the candidates as providers list them, without resolving each one.
    /// Faster, but the output may include targets that do not exist.
    ///
    /// Under --json the rows are unconfirmed candidates: `addr`, `package`
    /// and `name` are filled in, and every field read from the target
    /// (`labels`, `driver`, `deps`, `hashin`) is null.
    #[arg(long)]
    pub candidates: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<QueryRow>,
}

/// `heph query --json`: one element per match, in the text output's order.
///
/// Plain `query` resolves every match's spec to confirm it exists, so the
/// spec's fields are free; under `--candidates` nothing is resolved and they
/// are null.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct QueryRow {
    /// The target's address.
    addr: Addr,
    /// Its package, as `heph inspect packages` prints it (`a/b`; empty at the
    /// root).
    package: String,
    /// Its name within the package.
    name: String,
    /// The labels the target declares; null under --candidates.
    labels: Option<Vec<String>>,
    /// The driver that runs it; null under --candidates.
    #[record(opt_in)]
    driver: Option<String>,
    /// Its declared inputs (`//pkg:name`, with an output group or filters
    /// when it names any), in the order the def declares them; null under
    /// --candidates.
    #[record(resolved)]
    deps: Option<Vec<TargetAddr>>,
    /// Its input hash, the key of its cache entry; null under --candidates.
    #[record(built)]
    hashin: Option<String>,
}

/// The fields of a [`QueryRow`] that cost something to fill in, decided once
/// from the selection.
#[derive(Debug, Clone, Copy)]
struct Wants {
    /// `labels` or `driver`: the spec, already resolved by plain `query`.
    spec: bool,
    labels: bool,
    driver: bool,
    deps: bool,
    hashin: bool,
}

impl Wants {
    fn new(out: &output::JsonArgs<QueryRow>, candidates: bool) -> Self {
        // A candidate is never resolved, so nothing read from the target is.
        let wants = |field| !candidates && out.wants(field);
        let labels = wants(QueryRow::LABELS);
        let driver = wants(QueryRow::DRIVER);
        Self {
            spec: labels || driver,
            labels,
            driver,
            deps: wants(QueryRow::DEPS),
            hashin: wants(QueryRow::HASHIN),
        }
    }
}

/// The [`QueryRow`] for `addr`, computing only what `wants` names (I4).
async fn query_row(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    wants: Wants,
    addr: Addr,
) -> anyhow::Result<QueryRow> {
    let spec = if wants.spec {
        // A memo hit: `query_existing` resolved it to confirm the match.
        Some(Arc::clone(&engine).get_spec(rs.clone(), &addr).await?)
    } else {
        None
    };
    let deps = if wants.deps {
        let def = Arc::clone(&engine).get_def(rs.clone(), &addr).await?;
        Some(
            def.target_def
                .inputs
                .iter()
                .map(|input| input.r#ref.clone())
                .collect(),
        )
    } else {
        None
    };
    let hashin = if wants.hashin {
        Some(engine.meta(rs, &addr).await?.hashin)
    } else {
        None
    };
    Ok(QueryRow {
        package: addr.package.as_str().to_owned(),
        name: addr.name.clone(),
        labels: spec
            .as_ref()
            .filter(|_| wants.labels)
            .map(|s| s.labels.clone()),
        driver: spec
            .as_ref()
            .filter(|_| wants.driver)
            .map(|s| s.driver.clone()),
        deps,
        hashin,
        addr,
    })
}

/// Push `query --json`'s row for each of `addrs` into `list` as it is found,
/// computing only the fields `out` selects (I4). Rows are filled in a
/// bounded window and yielded in the walk's order, the text output's (D9). A
/// closed stdout stops the walk: `addrs` is dropped unpolled. The list is
/// left open; the caller closes it when the walk was whole (D10).
pub async fn push_json_rows<W: std::io::Write>(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    out: &output::JsonArgs<QueryRow>,
    candidates: bool,
    addrs: impl Stream<Item = anyhow::Result<Addr>>,
    list: &mut output::ListWriter<'_, QueryRow, W>,
) -> anyhow::Result<output::Flow> {
    let wants = Wants::new(out, candidates);
    let rows = addrs
        .map_ok(|addr| query_row(Arc::clone(engine), rs.clone(), wants, addr))
        .try_buffered(crate::engine::fanout::discovery_concurrency());
    list.push_stream(rows).await
}

/// The addrs `query` prints for `m`, in the walk's order: by default only
/// targets that exist, each match resolved to confirm it; with `candidates`,
/// the listing-level candidates unconfirmed. A candidate the walk rules out is
/// never resolved in either mode.
pub fn select<'a>(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    m: &'a Matcher,
    discovery: Discovery,
    candidates: bool,
) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
    if candidates {
        engine.query(rs, m, discovery).left_stream()
    } else {
        engine.query_existing(rs, m, discovery).right_stream()
    }
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let m = resolve_matcher(&args.expr, &args.arg1, &args.arg2, &cwp, true)?;
    let candidates = args.candidates;
    let json = args.out.clone();
    run_engine_command_out(
        format!("Querying {}", crate::htquery::format(&m)),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // What matched goes to stdout as it is found; what the walk could not
            // resolve is reported on stderr at the end, and fails the command.
            // One address whose source file did not evaluate: the file's box
            // and exit 1, as `run` gives, not a gap; `--json` prints `[]`,
            // since nothing matched (D5).
            if !candidates
                && let Matcher::Addr(addr) = &m
                && Arc::clone(&engine)
                    .record_file_culprit(rs.clone(), addr)
                    .await
            {
                if json.enabled() {
                    json.printer()
                        .start_list()?
                        .finish(&|| rs.ctoken().is_cancelled())?;
                }
                return crate::commands::errors::finalize(&ctx, &rs, Ok(()), None, |()| Ok(()))
                    .await;
            }
            let gaps = Gaps::new(crate::htquery::format(&m));
            let stream = select(
                Arc::clone(&engine),
                rs.clone(),
                &m,
                Discovery::keep_going_unless(rs.fail_fast(), &gaps),
                candidates,
            );
            if json.enabled() {
                let cancelled = || rs.ctoken().is_cancelled();
                let mut list = json.printer().start_list()?;
                let res = push_json_rows(&engine, &rs, &json, candidates, stream, &mut list).await;
                return crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |flow| {
                    // Closed only when the walk is whole (D10).
                    if flow == output::Flow::Continue && gaps.is_empty() {
                        list.finish(&cancelled)?;
                    }
                    Ok(())
                })
                .await;
            }
            tokio::pin!(stream);

            // Output is incremental, so addrs are printed before `finalize` runs; a
            // provider running a target mid-stream records rich failures in `rs`, which
            // `finalize` renders after the addrs already flushed. A whole-graph (`//...`)
            // query records the total graph size for telemetry inside `Engine::query`.
            let out = BufferedStdout::new(&ctx);
            let res: anyhow::Result<()> = async {
                while let Some(addr) = stream.try_next().await? {
                    out.println(addr.format());
                }
                Ok(())
            }
            .await;
            out.close().await;

            crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(())).await
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() {
        let addr = crate::htaddr::parse_addr("//a/b:c").unwrap();
        let dep = TargetAddr {
            r#ref: crate::htaddr::parse_addr("//d:e").unwrap(),
            output: None,
            filters: Vec::new(),
        };
        output::check_keys(&QueryRow {
            package: "a/b".into(),
            name: "c".into(),
            labels: Some(vec!["test".into()]),
            driver: Some("exec".into()),
            deps: Some(vec![dep]),
            hashin: Some("h".into()),
            addr,
        })
        .unwrap();
    }

    /// The default set is what plain `query` already has; `--candidates`
    /// reads nothing from the target, whatever is selected.
    #[test]
    fn wants_follow_the_selection_and_candidates() {
        let defaults = output::JsonArgs::<QueryRow>::try_parse_from(["--json"]).unwrap();
        let w = Wants::new(&defaults, false);
        assert!(w.spec && w.labels);
        assert!(!w.driver && !w.deps && !w.hashin);

        let all = output::JsonArgs::<QueryRow>::try_parse_from(["--fields", "all"]).unwrap();
        let w = Wants::new(&all, false);
        assert!(w.spec && w.labels && w.driver && w.deps && w.hashin);
        let w = Wants::new(&all, true);
        assert!(!w.spec && !w.labels && !w.driver && !w.deps && !w.hashin);

        let addr_only = output::JsonArgs::<QueryRow>::try_parse_from(["--fields", "addr"]).unwrap();
        assert!(!Wants::new(&addr_only, false).spec);
    }
}
