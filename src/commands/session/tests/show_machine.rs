//! `heph session show`'s machine line and `--json` `machine` (spec row
//! `show_prints_machine_line`).

use hcore::events::PhaseSpan;
use hcore::units::human_bytes;
use serde_json::json;

use super::*;
use crate::tui::session::format::{MachineInfoRecord, MachineSampleRecord, MachineSourceFailure};
use crate::tui::session::store;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// The session's start, in microseconds: when the sampler read the
/// start-time figures.
const T0_US: u64 = T0 * 1_000;
const SEC: u64 = 1_000_000;

fn info(cpus: u32, limit_ucpus: u64, failed: &[(&str, &str)]) -> SessionRecord {
    SessionRecord::session(
        T0,
        SessionRecordKind::MachineInfo(Box::new(MachineInfoRecord {
            time_unix_us: T0_US,
            cpu_count: Some(cpus),
            cpu_limit_ucpus: Some(limit_ucpus),
            cpu_limit_source: None,
            memory_limit_bytes: Some(16 * GIB),
            memory_limit_source: None,
            failed: failures(failed),
        })),
    )
}

fn failures(failed: &[(&str, &str)]) -> Vec<MachineSourceFailure> {
    failed
        .iter()
        .map(|&(source, error)| MachineSourceFailure {
            source: source.to_owned(),
            error: error.to_owned(),
        })
        .collect()
}

/// A sample `secs` seconds into the run, covering the second before it.
fn sample(
    secs: u64,
    busy_ppm: u32,
    used: Option<u64>,
    resident: u64,
    failed: &[(&str, &str)],
) -> SessionRecord {
    SessionRecord::session(
        T0 + secs * 1_000,
        SessionRecordKind::MachineSample(Box::new(MachineSampleRecord {
            time_unix_us: T0_US + secs * SEC,
            interval_us: SEC,
            cpu_busy_ppm: Some(busy_ppm),
            memory_used_bytes: used,
            process_cpu_us: Some(secs * 100_000),
            process_resident_bytes: Some(resident),
            failed: failures(failed),
            dropped: 0,
        })),
    )
}

/// `//a:b`'s result, with one phase span per `(name, start, end)`, in
/// microseconds into the run.
fn result_with(waits: &[(&'static str, u64, u64)]) -> SessionRecord {
    record(
        T0 + 3_900,
        BuildEventKind::ResultEnd {
            spans: waits
                .iter()
                .map(|&(name, start, end)| PhaseSpan {
                    name: name.into(),
                    start_unix_us: T0_US + start,
                    end_unix_us: T0_US + end,
                })
                .collect(),
            left_behind: None,
            addr: "//a:b".into(),
            error: None,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
            cancelled: false,
        },
    )
}

/// A whole run: `//a:b` from build_records, `machine`, its result, `End`.
fn run(machine: Vec<SessionRecord>, waits: &[(&'static str, u64, u64)]) -> Vec<SessionRecord> {
    let mut records = build_records("//a:b", T0, None);
    records.extend(machine);
    records.push(result_with(waits));
    records
}

fn ended_run(records: &mut Vec<SessionRecord>) {
    records.push(SessionRecord::session(
        T0 + 4_000,
        SessionRecordKind::End {
            exit_code: Some(0),
            panicked: false,
        },
    ));
}

async fn show_both(sources: &[&dyn SessionSource], id: SessionId) -> (String, serde_json::Value) {
    let text = run_show(sources, &id.encode(), false).await.expect("show");
    let json = run_show(sources, &id.encode(), true)
        .await
        .expect("show --json");
    (
        text,
        serde_json::from_str::<serde_json::Value>(&json).expect("json")["machine"].clone(),
    )
}

/// The line starting `machine: `, its indented hint, and every sampler line
/// after it.
fn machine_lines(out: &str) -> Vec<&str> {
    out.lines()
        .skip_while(|l| !l.starts_with("machine: "))
        .take_while(|l| {
            l.starts_with("machine: ")
                || l.starts_with("  admission waited")
                || l.starts_with("the sampler stopped")
                || l.starts_with("the sampler did not start")
        })
        .collect()
}

/// Spec row `show_prints_machine_line`: text and `--json` for sampled,
/// partial, failed and none (each reason); a figure with no sample is
/// absent; quota and host labels; a live session's figures are the ones its
/// seal would write.
/// A live session's busy mean weights each sample by the interval it covers,
/// as the seal does: a tick the sampler dropped (or a sample with no busy
/// figure) adds no weight to the next one. Busy 1, 1, 0 after a dropped
/// tick, none, then 1 is 3/4 on both sides, not the 2/3 that weighting by
/// the gap between busy rows gives.
#[tokio::test]
async fn live_busy_mean_weights_like_the_seal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let machine = || {
        let mut after_drop = sample(4, 0, Some(GIB), MIB, &[]);
        if let Some(SessionRecordKind::MachineSample(s)) = &mut after_drop.session {
            s.dropped = 1;
        }
        // A tick whose CPU counters did not move: no busy figure.
        let mut still = sample(5, 0, Some(GIB), MIB, &[]);
        if let Some(SessionRecordKind::MachineSample(s)) = &mut still.session {
            s.cpu_busy_ppm = None;
        }
        vec![
            info(8, 8_000_000, &[]),
            sample(1, 1_000_000, Some(GIB), MIB, &[]),
            sample(2, 1_000_000, Some(GIB), MIB, &[]),
            after_drop,
            still,
            sample(6, 1_000_000, Some(GIB), MIB, &[]),
        ]
    };
    let sealed_id = SessionId::new(T0, 1);
    let mut records = run(machine(), &[]);
    ended_run(&mut records);
    write_ended(
        &root,
        &meta(sealed_id, 1, &["heph", "run", "//a:b"]),
        &records,
        &ended(Some(0), T0 + 4_000),
    );
    let live_id = SessionId::new(T0, 2);
    let _held = write_live(
        &root,
        &meta(live_id, 2, &["heph", "run", "//a:b"]),
        &run(machine(), &[]),
        None,
    );
    let (_, sealed) = show_both(&sources, sealed_id).await;
    let (_, live) = show_both(&sources, live_id).await;
    let mean = |m: &serde_json::Value| m["cpu_busy_mean"].as_f64().expect("a mean");
    assert!(
        (mean(&sealed) - 0.75).abs() < 1e-9,
        "sealed, 3 busy seconds of 4: {sealed}"
    );
    assert!(
        (mean(&live) - mean(&sealed)).abs() < 1e-9,
        "live {live} vs sealed {sealed}"
    );
}

#[tokio::test]
async fn show_prints_machine_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let mut pid = 0;
    let mut write = |records: Vec<SessionRecord>, seal: bool| {
        pid += 1;
        // Every session starts at T0, told apart by its random byte.
        let id = SessionId::new(T0, pid);
        let m = meta(id, u32::from(pid), &["heph", "run", "//a:b"]);
        let live = if seal {
            let mut records = records;
            ended_run(&mut records);
            write_ended(&root, &m, &records, &ended(Some(0), T0 + 4_000));
            None
        } else {
            Some(write_live(&root, &m, &records, None))
        };
        (id, live)
    };

    // Sampled, on the host: three one-second samples, the middle one busy.
    // The admission wait sits in the busy second; the deps wait is half in
    // a spare second, half past the last sample.
    let machine = || {
        vec![
            info(8, 8_000_000, &[]),
            sample(1, 500_000, Some(8 * GIB), 100 * MIB, &[]),
            sample(2, 950_000, Some(8 * GIB), 200 * MIB, &[]),
            sample(3, 500_000, Some(12 * GIB), 150 * MIB, &[]),
        ]
    };
    let waits = [
        ("heph.admission", 1_200_000, 1_400_000),
        ("heph.deps", 2_500_000, 3_500_000),
    ];
    let (sampled, _) = write(run(machine(), &waits), true);
    let (text, m) = show_both(&sources, sampled).await;
    assert_eq!(
        machine_lines(&text),
        [format!(
            "machine: 8 CPUs (host), {} RAM (host); CPU busy 65% mean, 95% peak; memory used {} \
             peak (75% of RAM); heph resident {} peak; waiting: admission 0.2s (0.2s busy), \
             deps 1.0s (0.5s spare, 0.5s unknown)",
            human_bytes(16 * GIB),
            human_bytes(12 * GIB),
            human_bytes(200 * MIB),
        )],
        "{text}"
    );
    let mean = m["cpu_busy_mean"].as_f64().expect("a mean");
    assert!((mean - 0.65).abs() < 1e-9, "time-weighted: {m}");
    let mut rest = m.clone();
    rest.as_object_mut()
        .expect("object")
        .remove("cpu_busy_mean");
    assert_eq!(
        rest,
        json!({
            "status": "sampled",
            "cpus": 8,
            "cpu_limit": 8.0,
            "cpu_limit_source": "host",
            "memory_limit_bytes": 16 * GIB,
            "memory_limit_source": "host",
            "cpu_busy_peak": 0.95,
            "memory_used_peak_bytes": 12 * GIB,
            "heph_resident_peak_bytes": 200 * MIB,
            "sources_failed": [],
            "waiting": {
                "threshold_cpu": 0.9,
                "threshold_memory": 0.95,
                "by_phase": {
                    "admission": {"busy_us": 200_000, "spare_us": 0, "unknown_us": 0},
                    "deps": {"busy_us": 0, "spare_us": 500_000, "unknown_us": 500_000},
                },
            },
        }),
        "no reason when sampled: {m}"
    );

    // The same run, live: no summary rows yet, the same figures from the
    // samples so far.
    let (live, _held) = write(run(machine(), &waits), false);
    let (live_text, live_m) = show_both(&sources, live).await;
    assert_eq!(live_m, m, "a live session's figures are its seal's");
    assert_eq!(machine_lines(&live_text), machine_lines(&text));

    // The admission hint: admission waited mostly (and over 1 s) with the
    // machine to spare. Only admission earns it, and only past 1 s: a deps
    // wait as spare, or an admission wait of exactly 1 s spare, has none.
    // The first sample opens the timeline: the first second is unknown.
    let idle = || {
        vec![
            info(8, 8_000_000, &[]),
            sample(1, 300_000, Some(4 * GIB), 100 * MIB, &[]),
            sample(2, 300_000, Some(4 * GIB), 100 * MIB, &[]),
            sample(3, 300_000, Some(4 * GIB), 100 * MIB, &[]),
        ]
    };
    let (hinted, _) = write(run(idle(), &[("heph.admission", 500_000, 3_000_000)]), true);
    let (text, _) = show_both(&sources, hinted).await;
    let lines = machine_lines(&text);
    assert_eq!(
        lines.get(1..),
        Some(
            &[
                "  admission waited 2.0s of 2.5s with the machine to spare: the pools' capacity \
                 or the targets' declared `resources` held them back, not the machine"
            ][..]
        ),
        "{text}"
    );
    let (unhinted, _) = write(
        run(
            idle(),
            &[
                ("heph.admission", 1_000_000, 2_000_000),
                ("heph.deps", 0, 3_000_000),
            ],
        ),
        true,
    );
    let (text, m) = show_both(&sources, unhinted).await;
    assert_eq!(machine_lines(&text).len(), 1, "{text}");
    assert_eq!(
        m["waiting"]["by_phase"],
        json!({
            "admission": {"busy_us": 0, "spare_us": SEC, "unknown_us": 0},
            "deps": {"busy_us": 0, "spare_us": 2 * SEC, "unknown_us": SEC},
        }),
        "{m}"
    );

    // Partial, under a quota: the memory source stopped at the first
    // sample, so the memory peak is absent rather than 0.
    let stopped = (
        "heph.machine.memory.used",
        "no MemAvailable in /proc/meminfo",
    );
    let (partial, _) = write(
        run(
            vec![
                info(8, 2_500_000, &[]),
                sample(1, 250_000, None, 100 * MIB, &[stopped]),
                sample(2, 750_000, None, 100 * MIB, &[]),
            ],
            &[],
        ),
        true,
    );
    let (text, m) = show_both(&sources, partial).await;
    assert_eq!(
        machine_lines(&text),
        [
            format!(
                "machine: 2.5 CPUs (quota, of 8), {} RAM (host); CPU busy 50% mean, 75% peak; \
                 heph resident {} peak",
                human_bytes(16 * GIB),
                human_bytes(100 * MIB),
            ),
            "the sampler stopped reading heph.machine.memory.used 1s into the run: no \
             MemAvailable in /proc/meminfo"
                .to_owned(),
        ],
        "{text}"
    );
    assert_eq!(m["status"], "partial", "{m}");
    assert_eq!(m["cpu_limit"], 2.5, "{m}");
    assert_eq!(m["cpu_limit_source"], "quota", "{m}");
    assert_eq!(m["memory_limit_source"], "host", "{m}");
    assert!(m.get("memory_used_peak_bytes").is_none(), "absent: {m}");
    assert!(m.get("reason").is_none(), "{m}");
    assert_eq!(
        m["sources_failed"],
        json!([{
            "source": "heph.machine.memory.used",
            "error": "no MemAvailable in /proc/meminfo",
            "at_unix_us": T0_US + SEC,
        }]),
    );
    assert_eq!(m["waiting"]["by_phase"], json!({}), "nothing waited: {m}");

    // Failed: the start-time figures, a source stopped, no sample.
    let (failed, _) = write(
        run(
            vec![info(
                8,
                8_000_000,
                &[("heph.machine.cpu.busy", "host_statistics64: 5")],
            )],
            &[],
        ),
        true,
    );
    let (text, m) = show_both(&sources, failed).await;
    assert_eq!(
        machine_lines(&text),
        [
            format!(
                "machine: none: the sampler's sources stopped before a sample (see below); \
                 8 CPUs (host), {} RAM (host)",
                human_bytes(16 * GIB),
            ),
            "the sampler stopped reading heph.machine.cpu.busy 0s into the run: \
             host_statistics64: 5"
                .to_owned(),
        ],
        "{text}"
    );
    assert_eq!(m["status"], "failed", "{m}");
    assert!(m.get("reason").is_none(), "{m}");
    for absent in [
        "cpu_busy_mean",
        "cpu_busy_peak",
        "memory_used_peak_bytes",
        "heph_resident_peak_bytes",
    ] {
        assert!(m.get(absent).is_none(), "{absent}: {m}");
    }

    // None, shorter than one tick: CPUs and RAM only.
    let (short, _) = write(run(vec![info(8, 8_000_000, &[])], &[]), true);
    let (text, m) = show_both(&sources, short).await;
    assert_eq!(
        machine_lines(&text),
        [format!(
            "machine: none: no samples (shorter than one tick); 8 CPUs (host), {} RAM (host)",
            human_bytes(16 * GIB),
        )],
        "{text}"
    );
    assert_eq!(m["status"], "none", "{m}");
    assert_eq!(m["reason"], "no_samples", "{m}");
    assert_eq!(m["cpus"], 8, "{m}");
    assert!(m.get("cpu_busy_mean").is_none(), "{m}");

    // Failed, a sampler that never started: the recorder writes its failure
    // as a source named `sampler`, so it is not read as an older heph's.
    // The wait reads unknown.
    let admission = [("heph.admission", 1_000_000, 2_000_000)];
    let not_started = SessionRecord::session(
        T0,
        SessionRecordKind::MachineInfo(Box::new(MachineInfoRecord {
            time_unix_us: T0_US,
            failed: failures(&[("sampler", "Resource temporarily unavailable")]),
            ..MachineInfoRecord::default()
        })),
    );
    let (unsampled, _) = write(run(vec![not_started], &admission), true);
    let (text, m) = show_both(&sources, unsampled).await;
    assert_eq!(
        machine_lines(&text),
        [
            "machine: none: the sampler did not start (see below); \
             waiting: admission 1.0s (1.0s unknown)",
            "the sampler did not start: Resource temporarily unavailable \
             (build unaffected; no machine data recorded)",
        ],
        "{text}"
    );
    assert_eq!(m["status"], "failed", "{m}");
    assert!(m.get("reason").is_none(), "{m}");
    assert!(m.get("cpus").is_none(), "{m}");
    assert_eq!(m["sources_failed"][0]["source"], "sampler", "{m}");
    assert_eq!(
        m["waiting"]["by_phase"]["admission"],
        json!({"busy_us": 0, "spare_us": 0, "unknown_us": 1_000_000}),
    );

    // None, recorded before machine stats: no machine record at all, as PR 4
    // derived it (derivation 1) and as re-derived since (show repairs it).
    for derivation in [Some(1), None] {
        let (older, _) = write(run(Vec::new(), &admission), true);
        if let Some(d) = derivation {
            let conn = rusqlite::Connection::open(root.join(older.encode()).join(store::FILE))
                .expect("open the recording");
            conn.execute("UPDATE session SET derivation = ?1", [d])
                .expect("as PR 4 derived it");
        }
        let (text, m) = show_both(&sources, older).await;
        assert_eq!(
            machine_lines(&text),
            [
                "machine: none: recorded before machine stats (no machine record); \
                 waiting: admission 1.0s (1.0s unknown)"
            ],
            "{derivation:?}: {text}"
        );
        assert_eq!(m["status"], "none", "{m}");
        assert_eq!(m["reason"], "older_derivation", "{m}");
        assert_eq!(m["sources_failed"], json!([]), "{m}");
    }
}

/// D30's show totals (spec `show_prints_pool_lines`, PR 5 half): a `cpu`
/// pool's line ends with its `resources_vs_machine` totals and its `--json`
/// entry carries them; a pool with no machine counterpart has neither.
#[tokio::test]
async fn show_prints_pool_vs_machine() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let pool = |name: &str, held: u64, capacity: u64| {
        record(
            T0,
            BuildEventKind::PoolUsage {
                pool: name.into(),
                held,
                capacity,
                waiting: 0,
                removals_pending: 0,
            },
        )
    };
    // The cpu pool full from the start, the machine 30% busy: the two
    // intervals the samples close are declared above use; the first second
    // (no sample opens it) and the last (after the last sample) unknown.
    let mut records = run(
        vec![
            pool("cpu", 8_000_000, 8_000_000),
            pool("docker", 1, 4),
            info(8, 8_000_000, &[]),
            sample(1, 300_000, Some(4 * GIB), 100 * MIB, &[]),
            sample(2, 300_000, Some(4 * GIB), 100 * MIB, &[]),
            sample(3, 300_000, Some(4 * GIB), 100 * MIB, &[]),
        ],
        &[],
    );
    ended_run(&mut records);
    let id = SessionId::new(T0, 1);
    write_ended(
        &root,
        &meta(id, 1, &["heph", "run", "//a:b"]),
        &records,
        &ended(Some(0), T0 + 4_000),
    );
    let file = crate::tui::session::FileSource::new(&root);
    let sources: [&dyn SessionSource; 1] = [&file];

    let text = run_show(&sources, &id.encode(), false).await.expect("show");
    let pools: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(" pool (") || l.starts_with("  against the machine"))
        .collect();
    assert_eq!(
        pools,
        [
            "cpu pool (capacity 8): held 8 mean while in use (100%), 8 peak; nothing waited \
             for it",
            // Declared above use for all the time compared, but nothing
            // waited for the pool: the declarations held nothing back, so
            // no advice to lower them.
            "  against the machine: 2.0s with the pool 90%+ held while the machine was under \
             50%, 2.0s not compared (no sample)",
            "docker pool (capacity 4): held 1 mean while in use (25%), 1 peak; nothing waited \
             for it",
        ],
        "{text}"
    );

    let json: serde_json::Value = serde_json::from_str(
        &run_show(&sources, &id.encode(), true)
            .await
            .expect("show --json"),
    )
    .expect("json");
    assert_eq!(
        json["pools"],
        json!([
            {
                "pool": "cpu", "unit": "{cpu}", "capacity": 8.0,
                "held_mean": 8.0, "held_peak": 8.0, "waiting_us": 0,
                "declared_above_use_us": 2_000_000, "use_above_declared_us": 0,
                "matched_us": 0, "unknown_us": 2_000_000,
            },
            {
                "pool": "docker", "unit": "1", "capacity": 4.0,
                "held_mean": 1.0, "held_peak": 1.0, "waiting_us": 0,
            },
        ]),
        "{json}"
    );
}

/// The pool's second line: plain words for each non-zero total, and advice
/// only when one side was out of step for over half the time compared
/// (unknown time is not compared). The advice to lower comes only when
/// something waited for the pool: with nothing queued, over-declaring cost
/// nothing.
#[test]
fn pool_versus_line_advises_only_a_majority() {
    use super::super::show::versus_line;
    use crate::tui::session::machine::VsMachine;
    let vs = |declared: u64, used: u64, matched: u64, unknown: u64| VsMachine {
        declared_above_use_us: declared * SEC,
        use_above_declared_us: used * SEC,
        matched_us: matched * SEC,
        unknown_us: unknown * SEC,
    };
    assert_eq!(versus_line("cpu", &vs(0, 0, 0, 0), true), "");
    assert_eq!(
        versus_line("cpu", &vs(3, 0, 2, 0), true),
        "  against the machine: 3.0s with the pool 90%+ held while the machine was under 50%, \
         2.0s in step; targets may declare more CPU than they use: consider lowering their \
         `resources`\n"
    );
    assert_eq!(
        versus_line("cpu", &vs(3, 0, 2, 0), false),
        "  against the machine: 3.0s with the pool 90%+ held while the machine was under 50%, \
         2.0s in step\n",
        "nothing waited: no advice to lower"
    );
    // Raising is advice whether or not anything waited: the machine, not the
    // pool, was the limit.
    assert_eq!(
        versus_line("memory", &vs(0, 3, 2, 9), false),
        "  against the machine: 3.0s with the machine 90%+ while the pool was under 50% held, \
         2.0s in step, 9.0s not compared (no sample); targets may use more memory than they \
         declare, or other work shares the machine: consider raising their `resources`\n"
    );
    // Half the compared time is not a majority: no advice either way.
    assert_eq!(
        versus_line("cpu", &vs(2, 0, 2, 0), true),
        "  against the machine: 2.0s with the pool 90%+ held while the machine was under 50%, \
         2.0s in step\n"
    );
    assert_eq!(
        versus_line("cpu", &vs(0, 0, 0, 4), true),
        "  against the machine: 4.0s not compared (no sample)\n"
    );
}
