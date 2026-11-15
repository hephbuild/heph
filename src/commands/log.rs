//! `heph log`: a target's log, by address.
//!
//! The log's bytes go to stdout as they are, and one status line for people
//! (when, from where) to stderr, so `| grep` sees only the log. Exit 0 when a
//! log was printed, whatever the target's outcome; 1 when there is none or
//! the command failed ([`NoLog`]).
//!
//! Without `--current` the command only reads (I10): it opens what this
//! workspace's heph and its caches hold through [`Engine::open_log`] /
//! [`Engine::follow_log`], and never resolves the graph, hashes, locks a
//! target or starts a run. `--current` is [`Engine::meta`]: it may resolve,
//! hash and build the dependencies the input hash needs, and never runs or
//! locks the target itself.

use std::collections::VecDeque;
use std::io::{self, BufRead as _, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use hcore::hasync::StdCancellationToken;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::log_api::TAIL_MAX;
use crate::engine::request_state::RequestState;
use crate::engine::{
    Engine, FollowStep, LogFollower, LogInfo, LogOptions, LogSource, LogUnavailable, NoLiveLogs,
    OpenLog,
};
use crate::htaddr::{self, Addr};
use crate::tui::session::{SessionRoot, SessionRunLogLocator, SessionSlot};

#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// Follow the run going on until it ends, or wait for one to start
    ///
    /// A run in a heph started with `HEPH_NO_SESSION=1` cannot be followed
    /// from another terminal.
    #[arg(short = 'f', long)]
    pub follow: bool,
    /// Print only the last N lines
    #[arg(short = 'n', long = "lines", value_name = "N")]
    pub lines: Option<usize>,
    /// The log of the run matching the target's current inputs
    ///
    /// Resolves the target and computes its input hash, building the
    /// dependencies that hash needs, but never runs the target itself.
    #[arg(long)]
    pub current: bool,
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
}

/// What [`show`] is asked for: the flags, without the address.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request {
    pub follow: bool,
    pub lines: Option<usize>,
    pub current: bool,
}

/// What [`show`] did.
#[derive(Debug)]
pub enum Shown {
    /// A log was found and printed; what it was.
    Log(LogInfo),
    /// None was; the status line said why.
    Nothing,
}

/// No log was printed. The status line already said why: `main` exits 1
/// and prints nothing more.
#[derive(Debug, thiserror::Error)]
#[error("no log")]
pub struct NoLog;

/// How long a follow sleeps when nothing is new.
const IDLE: Duration = Duration::from_millis(50);

/// While `-n` catches up on a followed log, the bytes held before they are
/// trimmed to the last N lines.
const CATCH_UP_TRIM: usize = 1024 * 1024;

pub fn execute(
    args: &Args,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let shown = bootstrap::block_on(execute_async(args.clone(), session, global.clone()))??;
    match shown {
        Shown::Log(_) => Ok(()),
        Shown::Nothing => Err(NoLog.into()),
    }
}

async fn execute_async(
    args: Args,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<Shown> {
    let base = crate::engine::get_cwp()?;
    let addr = htaddr::parse_addr_with_base(args.addr.as_ref(), &base)
        .with_context(|| format!("parse {}", args.addr))?;
    let (engine, _shutdown) = bootstrap::new_engine(&global, session)?;
    // Runs going on in other hephs: found through the sessions under HOME.
    // `HEPH_NO_SESSION` turns recording off, not looking.
    let locator = SessionRoot::for_reading(std::env::var_os("HOME").as_deref())
        .map(|root| Arc::new(SessionRunLogLocator::new(root)) as _);
    let opts = LogOptions {
        locator,
        fetch_remote: true,
        ..LogOptions::default()
    };
    let req = Request {
        follow: args.follow,
        lines: args.lines,
        current: args.current,
    };
    // Cancelled by the first ctrl-c, as every request of the engine is.
    let rs = engine.new_state();
    show(&engine, &rs, &addr, req, opts, io::stdout(), io::stderr()).await
}

/// Print `addr`'s log to `out` and its status line to `err`. `rs` is the
/// request: `--current` hashes under it, and its cancellation (ctrl-c) stops
/// a follow.
pub async fn show<O, E>(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    addr: &Addr,
    req: Request,
    mut opts: LogOptions,
    out: O,
    mut err: E,
) -> anyhow::Result<Shown>
where
    O: Write + Send + 'static,
    E: Write + Send + 'static,
{
    if req.current {
        let deps = Arc::clone(engine)
            .hash_dep_count(rs.clone(), addr)
            .await
            .with_context(|| format!("resolving the inputs of {addr}"))?;
        if deps > 0 {
            say(
                &mut err,
                &format!(
                    "building {deps} {} to hash {addr}",
                    plural(deps, "dependency", "dependencies")
                ),
            );
        }
        let meta = Arc::clone(engine)
            .meta(rs.clone(), addr)
            .await
            .with_context(|| format!("computing the input hash of {addr}"))?;
        opts.hashin = Some(meta.hashin);
    }
    let found = if req.follow {
        follow(engine, addr, &opts)
            .await
            .map(|f| f.map(Found::Follow))
    } else {
        engine
            .open_log(addr, &opts)
            .await
            .map(|l| l.map(Found::Open))
    };
    let found = match found {
        Ok(Some(found)) => found,
        Ok(None) => {
            say(&mut err, &no_log(addr, opts.hashin.as_deref()));
            return Ok(Shown::Nothing);
        }
        Err(e) => {
            if let Some(e) = e.downcast_ref::<NoLiveLogs>() {
                say(
                    &mut err,
                    &format!(
                        "heph: no log for {addr}; session {} runs a heph that records no live logs",
                        e.session
                    ),
                );
                return Ok(Shown::Nothing);
            }
            if let Some(e) = e.downcast_ref::<LogUnavailable>() {
                say(
                    &mut err,
                    &format!(
                        "{} · log unavailable: {}",
                        status(addr, &e.info, hcore::events::now_unix_ms()),
                        e.reason
                    ),
                );
                return Ok(Shown::Nothing);
            }
            return Err(e.context(format!("reading the log of {addr}")));
        }
    };
    let (addr, ctoken) = (addr.clone(), rs.ctoken().clone());
    // A follow lasts as long as the run, and stdout may block on a slow
    // reader: a thread of its own, not one of the engine's bounded blocking
    // jobs.
    tokio::task::spawn_blocking(move || match found {
        Found::Open(log) => {
            let info = log.info().clone();
            say(
                &mut err,
                &status(&addr, &info, hcore::events::now_unix_ms()),
            );
            print_open(log, req.lines, out)
                .with_context(|| format!("printing the log of {addr}"))?;
            Ok(Shown::Log(info))
        }
        Found::Follow(f) => print_followed(f, &addr, req.lines, &ctoken, out, err),
    })
    .await
    .context("printing the log")?
}

enum Found {
    Open(OpenLog),
    Follow(LogFollower),
}

/// The follower `-f` reads: the run going on, else the newest finished log
/// read through once, else a wait for a run to start.
async fn follow(
    engine: &Arc<Engine>,
    addr: &Addr,
    opts: &LogOptions,
) -> anyhow::Result<Option<LogFollower>> {
    if let Some(f) = engine.follow_log(addr, opts, false).await? {
        return Ok(Some(f));
    }
    engine.follow_log(addr, opts, true).await
}

/// Print a whole log, or its last `lines` lines. A closed stdout ends it.
fn print_open(mut log: OpenLog, lines: Option<usize>, mut out: impl Write) -> io::Result<()> {
    let res = match lines {
        None => io::copy(&mut log.reader()?, &mut out).map(drop),
        Some(0) => Ok(()),
        Some(n) => {
            let len = log.len();
            let tail = log.tail(TAIL_MAX)?;
            match last_lines_start(&tail, n) {
                Some(start) => out.write_all(tail.get(start..).unwrap_or_default()),
                None if tail.len() as u64 >= len => out.write_all(&tail),
                // Fewer than N lines in the tail of a longer log: read it
                // through, holding only the last N lines.
                None => write_lines(&last_lines(log.reader()?, n)?, &mut out),
            }
        }
    };
    ignore_closed(res.and_then(|()| out.flush()))
}

/// Follow `f` to its end, or until `ctoken` is cancelled or stdout closes.
fn print_followed(
    mut f: LogFollower,
    addr: &Addr,
    lines: Option<usize>,
    ctoken: &StdCancellationToken,
    mut out: impl Write,
    mut err: impl Write,
) -> anyhow::Result<Shown> {
    let mut found = f.info();
    match &found {
        Some(info) => say(&mut err, &following(addr, info)),
        None => say(&mut err, &format!("{addr} · waiting for a run to start")),
    }
    // `-n`: the bytes already there, held until caught up, then only their
    // last N lines printed.
    let mut held = lines.map(|_| Vec::new());
    let mut live = found.as_ref().is_some_and(is_running);
    loop {
        if ctoken.is_cancelled() {
            return Ok(match found {
                Some(info) => Shown::Log(info),
                None => {
                    say(&mut err, &format!("{addr} · stopped before a run started"));
                    Shown::Nothing
                }
            });
        }
        let step = f
            .poll()
            .with_context(|| format!("following the log of {addr}"))?;
        if found.is_none()
            && let Some(info) = f.info()
        {
            say(&mut err, &following(addr, &info));
            live = is_running(&info);
            found = Some(info);
        }
        let caught_up = !matches!(step, FollowStep::Bytes(_));
        if let (Some(n), true) = (lines, caught_up)
            && let Some(bytes) = held.take()
        {
            let start = last_lines_start(&bytes, n).unwrap_or(0);
            let tail = bytes.get(start..).unwrap_or_default();
            if closed(out.write_all(tail).and_then(|()| out.flush()))? {
                break;
            }
        }
        match step {
            FollowStep::Bytes(bytes) => match (&mut held, lines) {
                (Some(buf), Some(n)) => {
                    buf.extend_from_slice(&bytes);
                    if buf.len() > CATCH_UP_TRIM
                        && let Some(start) = last_lines_start(buf, n)
                    {
                        buf.drain(..start);
                    }
                }
                _ => {
                    if closed(out.write_all(&bytes).and_then(|()| out.flush()))? {
                        break;
                    }
                }
            },
            FollowStep::Idle => std::thread::sleep(IDLE),
            FollowStep::End => {
                if live {
                    say(&mut err, &format!("{addr} · the run ended"));
                }
                break;
            }
        }
    }
    Ok(found.map_or(Shown::Nothing, Shown::Log))
}

fn is_running(info: &LogInfo) -> bool {
    matches!(info.source, LogSource::Live { running: true, .. })
}

fn following(addr: &Addr, info: &LogInfo) -> String {
    let line = status(addr, info, hcore::events::now_unix_ms());
    if is_running(info) {
        format!("{line} · following")
    } else {
        line
    }
}

/// The status line: where the log is from, and when. For people; not a
/// stable format.
fn status(addr: &Addr, info: &LogInfo, now_ms: u64) -> String {
    let age = hcore::units::human_duration(Duration::from_millis(
        now_ms.saturating_sub(info.at_unix_ms),
    ));
    match &info.source {
        LogSource::Live {
            running: true,
            session: Some(session),
        } => format!("{addr} · running {age} in session {session}"),
        LogSource::Live {
            running: true,
            session: None,
        } => format!("{addr} · running {age}"),
        LogSource::Live { running: false, .. } => {
            format!("{addr} · ran {age} ago, not cached")
        }
        LogSource::FailureSlot => format!("{addr} · failed {age} ago"),
        LogSource::Cache { remote } => format!(
            "{addr} · cached · from the run that filled the {}entry, {age} ago",
            if *remote { "remote " } else { "" }
        ),
    }
}

fn no_log(addr: &Addr, hashin: Option<&str>) -> String {
    match hashin {
        None => format!(
            "heph: no log for {addr}: it has not run in this workspace, and no cache entry for it kept one"
        ),
        Some(h) => format!(
            "heph: no log for {addr} at its current inputs ({h}): it has not run with them here, and no cache entry for them kept one"
        ),
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// One status line to stderr. Best-effort: a closed stderr must not fail
/// the command, whose output is the log.
fn say(err: &mut impl Write, line: &str) {
    drop(writeln!(err, "{line}").and_then(|()| err.flush()));
}

/// Where the last `n` lines of `buf` start, `None` when it holds fewer
/// line breaks than that (all of `buf` is then at most `n` lines). A final
/// newline ends the last line rather than starting another.
fn last_lines_start(buf: &[u8], n: usize) -> Option<usize> {
    if n == 0 {
        return Some(buf.len());
    }
    let body = buf.strip_suffix(b"\n").unwrap_or(buf);
    body.iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| **b == b'\n')
        .nth(n - 1)
        .map(|(i, _)| i + 1)
}

/// The last `n` lines of `r`, read through.
fn last_lines(r: impl io::Read, n: usize) -> io::Result<VecDeque<Vec<u8>>> {
    let mut r = BufReader::new(r);
    let mut lines = VecDeque::with_capacity(n.min(1024));
    if n == 0 {
        return Ok(lines);
    }
    loop {
        let mut line = Vec::new();
        if r.read_until(b'\n', &mut line)? == 0 {
            return Ok(lines);
        }
        if lines.len() == n {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

fn write_lines(lines: &VecDeque<Vec<u8>>, out: &mut impl Write) -> io::Result<()> {
    lines.iter().try_for_each(|l| out.write_all(l))
}

/// Whether a write failed because stdout was closed (`| head`): the reader
/// has what it wanted, which ends the command without an error.
fn closed(res: io::Result<()>) -> io::Result<bool> {
    match res {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e),
    }
}

fn ignore_closed(res: io::Result<()>) -> io::Result<()> {
    closed(res).map(drop)
}

#[cfg(test)]
mod tests {
    use super::{last_lines, last_lines_start};

    fn tail(buf: &[u8], n: usize) -> &[u8] {
        buf.get(last_lines_start(buf, n).unwrap_or(0)..)
            .unwrap_or_default()
    }

    #[test]
    fn last_lines_counts_a_final_newline_as_an_end() {
        assert_eq!(tail(b"a\nb\nc\nd\n", 3), b"b\nc\nd\n");
        assert_eq!(tail(b"a\nb\nc\nd", 3), b"b\nc\nd", "no trailing newline");
        assert_eq!(tail(b"a\nb\n", 3), b"a\nb\n", "N over the log");
        assert_eq!(last_lines_start(b"a\nb\n", 3), None);
        assert_eq!(tail(b"a\nb\n", 0), b"");
        assert_eq!(tail(b"", 2), b"");
        assert_eq!(tail(b"\n\n\n", 2), b"\n\n", "blank lines are lines");
    }

    #[test]
    fn last_lines_reads_through_keeping_n() {
        let got = last_lines(&b"1\n2\n3\n4"[..], 2).expect("read");
        assert_eq!(got, [b"3\n".to_vec(), b"4".to_vec()]);
    }
}
