use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use hcore::capacity::{Capacity, PoolSpec, Unit, Zero};
use hplugin::provider::PoolName;

/// The prefix of the per-pool capacity variables in heph's own environment:
/// `HEPH_RESOURCE_GPU=2` is `--resource gpu=2`.
pub const RESOURCE_ENV_PREFIX: &str = "HEPH_RESOURCE_";

/// A run's capacity for one pool, as written. Kept as text because a custom
/// pool's unit is the config's (`unit: bytes`), which is not known when the
/// command line is parsed; [`apply_resource_overrides`] parses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCapacity {
    pub value: String,
    /// Where it was written, as the user would type it (`-j 4`,
    /// `--resource gpu=2`, `HEPH_RESOURCE_GPU=2`), for the error message.
    pub source: String,
}

/// One layer of the run's pool overrides (environment or command line).
pub type ResourceOverrides = BTreeMap<PoolName, RawCapacity>;

/// One `--resource <name>=<n>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceOverride {
    pub pool: PoolName,
    pub value: String,
}

/// Check `value` early when `pool` is a builtin, whose unit no config can
/// change, so `-j abc` fails like any bad flag. A custom pool's value waits
/// for its configured unit.
fn check_builtin(pool: &str, value: &str) -> anyhow::Result<()> {
    if let Some(unit) = Unit::of_builtin(pool) {
        Capacity::parse(pool, unit, value, Zero::Refused)?;
    }
    Ok(())
}

/// Parse `-j/--jobs`: `--resource cpu=<n>`, so CPUs or a percentage.
fn parse_jobs(s: &str) -> Result<String, String> {
    check_builtin(PoolName::CPU, s).map_err(|e| format!("{e:#}"))?;
    Ok(s.to_string())
}

/// Parse `--resource <name>=<n>`. A run's override gives the pool a number,
/// so `0` is refused (a config may declare `gpu: 0`; the run sets it).
fn parse_resource_override(s: &str) -> Result<ResourceOverride, String> {
    let (name, value) = s
        .split_once('=')
        .ok_or_else(|| format!("{s:?} is not <name>=<n>: write e.g. gpu=2 or cpu=50%"))?;
    let pool = PoolName::try_from(name).map_err(|e| format!("{e:#}"))?;
    if value.is_empty() {
        return Err(format!("pool {name}: no value: write e.g. {name}=2"));
    }
    check_builtin(pool.as_str(), value).map_err(|e| format!("pool {name}: {e:#}"))?;
    Ok(ResourceOverride {
        pool,
        value: value.to_string(),
    })
}

/// The `HEPH_RESOURCE_<NAME>=<n>` overrides in `vars` (heph's environment).
/// `<NAME>` is lowercased and must then be a pool name; the value is checked
/// as a `--resource` value is. Any malformed one is an error naming the
/// variable, never a silently ignored setting.
pub fn env_resource_overrides(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> anyhow::Result<ResourceOverrides> {
    let mut out = ResourceOverrides::new();
    for (key, value) in vars {
        let Some(key) = key.to_str() else { continue };
        let Some(suffix) = key.strip_prefix(RESOURCE_ENV_PREFIX) else {
            continue;
        };
        let pool = PoolName::try_from(suffix.to_ascii_lowercase())
            .with_context(|| format!("{key} names no pool"))?;
        let value = value
            .to_str()
            .with_context(|| format!("{key} is not valid UTF-8"))?;
        if value.is_empty() {
            anyhow::bail!("{key} is empty: give the pool a capacity");
        }
        check_builtin(pool.as_str(), value).with_context(|| format!("{key}={value}"))?;
        let raw = RawCapacity {
            value: value.to_string(),
            source: format!("{key}={value}"),
        };
        if out.insert(pool, raw).is_some() {
            // Only reachable through case: `HEPH_RESOURCE_GPU` and `..._gpu`.
            anyhow::bail!("{key}: pool {suffix} is set by two variables");
        }
    }
    Ok(out)
}

/// Lay the run's overrides over the config's pools: flag, then environment,
/// then config. Each value is parsed in its pool's configured unit (a pool
/// the config does not declare takes its default unit), and must be above
/// 0. A pool in none of them keeps the engine's default.
pub fn apply_resource_overrides(
    config: &mut BTreeMap<PoolName, PoolSpec>,
    env: ResourceOverrides,
    flags: &ResourceOverrides,
) -> anyhow::Result<()> {
    let mut layered = env;
    layered.extend(flags.iter().map(|(p, r)| (p.clone(), r.clone())));
    for (pool, raw) in layered {
        let unit = config
            .get(&pool)
            .map_or_else(|| Unit::default_for(pool.as_str()), |s| s.unit);
        let capacity = Capacity::parse(pool.as_str(), unit, &raw.value, Zero::Refused)
            .with_context(|| format!("{} (pool {}, unit {unit})", raw.source, pool.as_str()))?;
        config
            .entry(pool)
            .or_insert(PoolSpec {
                unit,
                capacity: None,
            })
            .capacity = Some(capacity);
    }
    Ok(())
}

/// Parse `--stall-notice`: a duration, or `off` to disable.
///
/// Zero is the "off" sentinel rather than `Option`, because clap reads a field of
/// type `Option<T>` as "this flag is optional" and expects the parser to yield
/// `T` — returning an `Option` from the parser panics at access time.
fn parse_stall_notice(s: &str) -> Result<std::time::Duration, String> {
    if matches!(s.trim(), "off" | "none") {
        return Ok(std::time::Duration::ZERO);
    }
    humantime::parse_duration(s).map_err(|e| format!("invalid duration {s:?}: {e}"))
}

/// Global options shared by every subcommand. Flattened into the top-level CLI
/// with `global = true` so the flags are accepted before or after the
/// subcommand, then plumbed to each command's `execute`.
#[derive(Args, Clone, Debug, Default)]
pub struct GlobalOptions {
    /// Run against a fresh, empty scratch cache instead of the stored one.
    ///
    /// **Deletes nothing.** The stored cache is not touched, read, or emptied —
    /// the run is simply pointed at a throwaway directory instead, and that
    /// directory is discarded afterwards. A later ordinary build finds its cache
    /// exactly as it left it.
    ///
    /// This is how the scratch contract gets audited: a target's outputs must be
    /// identical whether its scratch is warm or cold, so a build with
    /// `--no-scratch` should produce the same `hashout`s as one without. If it
    /// does not, the target depends on carried-over state and is already broken.
    /// It implies `--force`: a scratch never reaches `hashin`, so without a
    /// rebuild the run would just replay the result built *with* a warm cache.
    ///
    /// Everything else is set up as normal — the directory is created and
    /// mounted, the environment variable is announced, the slot is locked — so a
    /// target reading `$MYCACHE` runs cold rather than failing on an unset
    /// variable. The audit is of your build, not of your shell.
    ///
    /// A build flag rather than a `heph tool scratch` subcommand — it modifies a
    /// build rather than being one, so it belongs next to `--force`. And a bool
    /// rather than `--scratch=on|off`: there are exactly two states, `off` is the
    /// only one anyone types, and a valued flag named `scratch` collides with
    /// every subcommand that wants a `--scratch` of its own (see
    /// `tool::clean`'s test).
    #[arg(long = "no-scratch", global = true)]
    pub no_scratch: bool,
    /// Sample CPU and write a pprof profile to PATH. `kill -USR2 <pid>`
    /// snapshots the profile so far without stopping the run — this is the point
    /// of the flag, since a hung build never reaches exit. A filtered final
    /// report is also written at exit.
    #[arg(long = "pprof-cpu", value_name = "PATH", global = true)]
    pub pprof_cpu: Option<PathBuf>,
    /// Write a diagnostic when a run makes no progress for this long
    ///
    /// heph watches its own event stream and, if nothing at all advances for the
    /// given duration while work is outstanding, appends one paragraph naming
    /// what is open, for how long, and whether any bytes are moving to
    /// `<home>/diag/stall-<pid>.log`, and logs the path at `warn`. Off with
    /// `--stall-notice=off`. Default: 60s.
    ///
    /// The text is a diagnostic, not a stable interface — parse the JSON surface
    /// instead.
    #[arg(
        long = "stall-notice",
        value_name = "DURATION",
        default_value = "60s",
        value_parser = parse_stall_notice,
        global = true
    )]
    pub stall_notice: std::time::Duration,
    /// Also dump every thread's backtrace on `SIGQUIT`. Can deadlock the process
    ///
    /// `SIGQUIT` always writes the in-flight report (what work is open and what
    /// it is waiting on), which is the half that names the stuck work and is
    /// written by an ordinary thread. This flag adds per-thread backtraces,
    /// which are captured *inside a signal handler* — that calls the DWARF
    /// unwinder and the allocator, neither of which is async-signal-safe. A
    /// thread interrupted inside `malloc` re-enters it from its own handler and
    /// deadlocks holding the arena lock, taking the whole process with it.
    ///
    /// Off by default because that outcome is the opposite of what a hang
    /// diagnostic is for. Turn it on only when the in-flight report was not
    /// enough, on a run you are willing to lose.
    #[arg(long = "diag-backtrace", global = true)]
    pub diag_backtrace: bool,
    /// Disable the interactive TUI (force CI/log-only output)
    #[arg(long = "no-tui", global = true)]
    pub no_tui: bool,
    /// Fail fast: stop at the first target failure instead of running every
    /// matched target and reporting all failures at the end
    #[arg(long = "fail-fast", visible_alias = "ff", global = true)]
    pub fail_fast: bool,
    /// Approve every `approval`-gated target without prompting. The notice (if
    /// any) is still printed in non-TUI mode.
    #[arg(long = "auto-approve", global = true)]
    pub auto_approve: bool,
    /// CPUs the run may reserve at once, not a target count
    ///
    /// The `cpu` pool's capacity for this run: CPUs (`-j 8`, `-j 2.5`), or a
    /// percentage of the default (`-j 50%`). Each target reserves what its
    /// `resources` declares (by default one CPU, 0.1 for IO-bound drivers
    /// such as `http`), and targets wait until
    /// their reservation fits. The same as `--resource cpu=<N>`; giving both
    /// is an error. Default: the config's `resources.cpu`, else the
    /// engine's parallelism. The model is in `docs/RESOURCES.md`.
    #[arg(
        short = 'j',
        long = "jobs",
        value_name = "N",
        value_parser = parse_jobs,
        global = true
    )]
    pub jobs: Option<String>,
    /// Set a resource pool's capacity for this run: `<name>=<n>`. Repeatable
    ///
    /// In the pool's unit: `cpu` in CPUs (`0.5`), `memory` as a size (`24G`),
    /// a custom pool as its configured `unit` (a count unless it says
    /// otherwise); `cpu` and `memory` also take a percentage of their default
    /// (`memory=75%`). Above 0. `sandbox` is the engine's own pool: it bounds
    /// sandboxes on disk (a count or a percentage of 4 per CPU).
    /// Overrides `HEPH_RESOURCE_<NAME>`, which
    /// overrides `resources.<name>` in `.hephconfig`. A pool may be given
    /// once per command line. See `docs/RESOURCES.md`.
    #[arg(
        long = "resource",
        value_name = "NAME=N",
        value_parser = parse_resource_override,
        global = true
    )]
    pub resources: Vec<ResourceOverride>,
}

impl GlobalOptions {
    /// The command line's pool capacities: every `--resource`, plus `-j` as
    /// `cpu`. A pool given twice — `-j 4 --resource cpu=8`, or `--resource`
    /// twice for one pool — is an error rather than "last wins": the two
    /// spellings have no order between them that the user would agree on.
    pub fn resource_overrides(&self) -> anyhow::Result<ResourceOverrides> {
        let jobs = self.jobs.as_ref().map(|value| RawCapacity {
            value: value.clone(),
            source: format!("-j {value}"),
        });
        let flags = self.resources.iter().map(|o| {
            let raw = RawCapacity {
                value: o.value.clone(),
                source: format!("--resource {}={}", o.pool.as_str(), o.value),
            };
            (&o.pool, raw)
        });
        let cpu = PoolName::cpu();
        let mut out = ResourceOverrides::new();
        for (pool, raw) in jobs.map(|j| (&cpu, j)).into_iter().chain(flags) {
            if out.insert(pool.clone(), raw).is_some() {
                let pool = pool.as_str();
                if pool == PoolName::CPU && self.jobs.is_some() {
                    anyhow::bail!(
                        "the cpu pool is set twice: -j/--jobs is --resource cpu=<n>, give one"
                    );
                }
                anyhow::bail!("pool {pool} is set twice: give --resource {pool}=<n> once");
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        global: GlobalOptions,
    }

    fn parse(args: &[&str]) -> GlobalOptions {
        TestCli::parse_from(args).global
    }

    #[test]
    fn fail_fast_is_opt_in() {
        // Default: fail-fast off — run every matched target, report all failures.
        assert!(!parse(&["heph"]).fail_fast);
        // Opt in with the long flag or its `ff` alias.
        assert!(parse(&["heph", "--fail-fast"]).fail_fast);
        assert!(parse(&["heph", "--ff"]).fail_fast);
    }

    #[test]
    fn no_tui_defaults_off() {
        assert!(!parse(&["heph"]).no_tui);
        assert!(parse(&["heph", "--no-tui"]).no_tui);
    }

    /// The command line's overrides, or the first error (clap's or the
    /// cross-flag check's).
    fn flags(args: &[&str]) -> anyhow::Result<ResourceOverrides> {
        let argv = std::iter::once("heph").chain(args.iter().copied());
        TestCli::try_parse_from(argv)?.global.resource_overrides()
    }

    fn env(vars: &[(&str, &str)]) -> anyhow::Result<ResourceOverrides> {
        env_resource_overrides(vars.iter().map(|(k, v)| (k.into(), v.into())))
    }

    fn pool(s: &str) -> PoolName {
        PoolName::try_from(s).unwrap()
    }

    type Specs = BTreeMap<PoolName, PoolSpec>;

    fn specs(rows: &[(&str, Unit, Option<Capacity>)]) -> Specs {
        rows.iter()
            .map(|(p, unit, capacity)| {
                (
                    pool(p),
                    PoolSpec {
                        unit: *unit,
                        capacity: *capacity,
                    },
                )
            })
            .collect()
    }

    /// The command line's overrides laid over `config`, parsed in its units.
    fn over(config: Specs, args: &[&str]) -> anyhow::Result<Specs> {
        let mut config = config;
        apply_resource_overrides(&mut config, ResourceOverrides::new(), &flags(args)?)?;
        Ok(config)
    }

    /// The command line's overrides with no config.
    fn overrides(args: &[&str]) -> anyhow::Result<Specs> {
        over(Specs::new(), args)
    }

    fn env_over(config: Specs, vars: &[(&str, &str)]) -> anyhow::Result<Specs> {
        let mut config = config;
        apply_resource_overrides(&mut config, env(vars)?, &ResourceOverrides::new())?;
        Ok(config)
    }

    const CPU: u64 = hcore::capacity::MICROS_PER_CPU;

    #[test]
    fn resource_override_parse_table() {
        use Capacity::Amount;
        use Unit::{Bytes, Count};
        let cpu = |n| specs(&[("cpu", Unit::Cpu, Some(Amount(n)))]);
        // Accepted: the pool's unit, as the config writes it.
        for (args, want) in [
            (&["-j", "4"][..], cpu(4 * CPU)),
            (&["--jobs=4"], cpu(4 * CPU)),
            (&["-j", "1.5"], cpu(CPU * 3 / 2)),
            (&["-j", "0.25"], cpu(CPU / 4)),
            (
                &["--resource", "gpu=2"],
                specs(&[("gpu", Count, Some(Amount(2)))]),
            ),
            (
                &["--resource", "memory=24G"],
                specs(&[("memory", Bytes, Some(Amount(24 << 30)))]),
            ),
            (
                &["--resource", "gpu=4294967296"],
                specs(&[("gpu", Count, Some(Amount(1 << 32)))]),
            ),
            (
                &["--resource", "gpu=1", "--resource", "docker=3"],
                specs(&[
                    ("gpu", Count, Some(Amount(1))),
                    ("docker", Count, Some(Amount(3))),
                ]),
            ),
            (&[], Specs::new()),
        ] {
            assert_eq!(overrides(args).unwrap(), want, "{args:?}");
        }
        // Refused: a run's override gives the pool a number above 0.
        for args in [
            &["-j", "0"][..],
            &["-j", "0.0"],
            &["--resource", "gpu=0"],
            &["--resource", "cpu=0"],
            &["-j", "-1"],
            &["--resource", "gpu=-1"],
            &["-j", "18446744073710"],
            &["--resource", "gpu=18446744073709551616"],
            &["-j", "1.5.0"],
            &["-j", "abc"],
            &["--resource", "gpu=abc"],
            &["--resource", "gpu=1.5"],
            &["--resource", "="],
            &["--resource", "gpu"],
            &["--resource", "gpu="],
            &["--resource", "=2"],
            &["--resource", "Gpu=2"],
            &["--resource", "a-b=2"],
            &["--resource", "gpu=1", "--resource", "gpu=2"],
        ] {
            assert!(overrides(args).is_err(), "{args:?} was accepted");
        }
        // A builtin's value is checked at parse, before any config is read.
        assert!(flags(&["-j", "abc"]).is_err());
        assert!(flags(&["--resource", "memory=lots"]).is_err());
        // A run never sets a pool to 0, and `sandbox` is no exception: its 0
        // is refused with the generic words, on the flag and in the
        // environment alike (a builtin's at parse, a custom pool's once its
        // unit is known).
        let zero = "\"0\" is not a capacity for a run: it is above 0";
        for pool in ["sandbox", "gpu"] {
            let err = format!(
                "{:#}",
                overrides(&["--resource", &format!("{pool}=0")]).unwrap_err()
            );
            assert!(err.contains(zero), "{pool}: {err}");
            let var = format!("HEPH_RESOURCE_{}", pool.to_uppercase());
            let err = format!(
                "{:#}",
                env_over(Specs::new(), &[(var.as_str(), "0")]).unwrap_err()
            );
            assert!(err.contains(&var) && err.contains(zero), "{pool}: {err}");
        }
        assert_eq!(
            overrides(&["--resource", "sandbox=10%"]).unwrap(),
            specs(&[("sandbox", Count, Some(Capacity::Percent(10)))])
        );
        // A custom pool's waits for its unit: `24G` is a count error, but
        // fine once the config says the pool is in bytes.
        assert!(flags(&["--resource", "vram=24G"]).is_ok());
        assert!(overrides(&["--resource", "vram=24G"]).is_err());
        assert_eq!(
            over(specs(&[("vram", Bytes, None)]), &["--resource", "vram=24G"]).unwrap(),
            specs(&[("vram", Bytes, Some(Amount(24 << 30)))])
        );
        // A custom pool in the cpu unit takes decimals.
        assert_eq!(
            over(
                specs(&[("lic", Unit::Cpu, None)]),
                &["--resource", "lic=0.5"]
            )
            .unwrap(),
            specs(&[("lic", Unit::Cpu, Some(Amount(CPU / 2)))])
        );
        // The error names the flag as written and the unit it was read in.
        let err = format!("{:#}", overrides(&["--resource", "vram=24G"]).unwrap_err());
        assert!(
            err.contains("--resource vram=24G") && err.contains("count"),
            "{err}"
        );

        // The environment: `<NAME>` lowercased, the value as `--resource`'s.
        assert_eq!(
            env_over(
                Specs::new(),
                &[
                    ("HEPH_RESOURCE_GPU", "2"),
                    ("HEPH_RESOURCE_CPU", "3"),
                    ("HEPH_RESOURCES", "nope"),
                    ("PATH", "/bin"),
                ]
            )
            .unwrap(),
            specs(&[
                ("gpu", Count, Some(Amount(2))),
                ("cpu", Unit::Cpu, Some(Amount(3 * CPU))),
            ])
        );
        for vars in [
            &[("HEPH_RESOURCE_GPU", "0")][..],
            &[("HEPH_RESOURCE_GPU", "-1")],
            &[("HEPH_RESOURCE_GPU", "abc")],
            &[("HEPH_RESOURCE_GPU", "")],
            &[("HEPH_RESOURCE_CPU", "abc")],
            &[("HEPH_RESOURCE_", "1")],
            &[("HEPH_RESOURCE_A-B", "1")],
            &[("HEPH_RESOURCE_1GPU", "1")],
            &[("HEPH_RESOURCE_GPU", "1"), ("HEPH_RESOURCE_gpu", "2")],
        ] {
            assert!(
                env_over(Specs::new(), vars).is_err(),
                "{vars:?} was accepted"
            );
        }
        // The error names the variable, not just the value.
        let err = format!(
            "{:#}",
            env_over(Specs::new(), &[("HEPH_RESOURCE_GPU", "0")]).unwrap_err()
        );
        assert!(err.contains("HEPH_RESOURCE_GPU"), "{err}");
    }

    #[test]
    fn jobs_is_sugar_for_resource_cpu() {
        for n in ["4", "50%", "0.5"] {
            let jobs = overrides(&["-j", n]).unwrap();
            assert_eq!(
                jobs,
                overrides(&["--resource", &format!("cpu={n}")]).unwrap()
            );
            assert_eq!(jobs, overrides(&["--jobs", n]).unwrap());
        }
        // Both spellings at once: an error naming both, not "last wins".
        let err = format!(
            "{:#}",
            overrides(&["-j", "4", "--resource", "cpu=8"]).unwrap_err()
        );
        assert!(
            err.contains("-j") && err.contains("--resource cpu"),
            "{err}"
        );
        // `-j` composes with other pools.
        assert_eq!(
            overrides(&["-j", "2", "--resource", "gpu=1"]).unwrap(),
            specs(&[
                ("cpu", Unit::Cpu, Some(Capacity::Amount(2 * CPU))),
                ("gpu", Unit::Count, Some(Capacity::Amount(1))),
            ])
        );
    }

    /// The pools' limits as the engine resolves them, by name. `None` is
    /// unbounded.
    fn limits(specs: &Specs, host_memory: u64) -> BTreeMap<String, Option<u64>> {
        crate::engine::resource_pool::resolve_capacities(specs, 8, || host_memory)
            .into_iter()
            .map(|(p, n)| (p.as_str().to_string(), n.bounded()))
            .collect()
    }

    #[test]
    fn override_precedence_is_flag_env_config_default() {
        use Capacity::Amount;
        use Unit::Count;
        let mut layered = specs(&[
            ("cpu", Unit::Cpu, Some(Amount(2 * CPU))),
            ("gpu", Count, Some(Amount(0))),
            ("docker", Count, Some(Amount(3))),
            ("lic", Count, None),
            ("slot", Count, None),
        ]);
        let env = env(&[("HEPH_RESOURCE_CPU", "4"), ("HEPH_RESOURCE_GPU", "5")]).unwrap();
        let flags = flags(&["-j", "6", "--resource", "slot=2"]).unwrap();
        apply_resource_overrides(&mut layered, env, &flags).unwrap();
        let want: BTreeMap<String, Option<u64>> = [
            ("cpu", Some(6 * CPU)),    // flag over env over config
            ("gpu", Some(5)),          // env over config's 0
            ("docker", Some(3)),       // config
            ("lic", None),             // declared with no value: unbounded
            ("slot", Some(2)),         // the run bounds an unbounded pool
            ("memory", Some(1 << 30)), // engine default
            ("sandbox", Some(24)),     // engine default: 4 per CPU of cpu
        ]
        .into_iter()
        .map(|(p, n)| (p.to_string(), n))
        .collect();
        assert_eq!(limits(&layered, 1 << 30), want);

        // Nothing written anywhere: the engine's defaults.
        let mut none = Specs::new();
        apply_resource_overrides(
            &mut none,
            ResourceOverrides::new(),
            &ResourceOverrides::new(),
        )
        .unwrap();
        assert_eq!(
            limits(&none, 7),
            [
                ("cpu", Some(8 * CPU)),
                ("memory", Some(7)),
                ("sandbox", Some(32))
            ]
            .into_iter()
            .map(|(p, n)| (p.to_string(), n))
            .collect()
        );
        // A host whose memory could not be read: memory is unbounded.
        assert_eq!(limits(&none, 0)["memory"], None);
    }

    #[test]
    fn percent_capacity_parse_table() {
        let resolve = |args: &[&str], host_memory: u64| {
            let mut out = BTreeMap::new();
            for (p, n) in limits(&overrides(args).unwrap(), host_memory) {
                out.insert(p, n.unwrap_or(u64::MAX));
            }
            out
        };
        // `-j 50%` on parallelism 8: 4 CPUs.
        assert_eq!(resolve(&["-j", "50%"], 1)["cpu"], 4 * CPU);
        // memory floors to bytes.
        assert_eq!(resolve(&["--resource", "memory=75%"], 3)["memory"], 2);
        // Over 100% is an overcommit by choice.
        assert_eq!(resolve(&["-j", "150%"], 1)["cpu"], 12 * CPU);
        assert_eq!(
            env_over(Specs::new(), &[("HEPH_RESOURCE_CPU", "50%")]).unwrap(),
            specs(&[("cpu", Unit::Cpu, Some(Capacity::Percent(50)))])
        );
        for args in [
            &["-j", "0%"][..],
            &["-j", "-50%"],
            &["-j", "50 %"],
            &["-j", "%"],
            &["-j", "50%%"],
            &["--resource", "memory=0%"],
            &["--resource", "gpu=50%"],
        ] {
            assert!(overrides(args).is_err(), "{args:?} was accepted");
        }
        assert!(env_over(Specs::new(), &[("HEPH_RESOURCE_GPU", "50%")]).is_err());
        // Percentages are for the builtins, which have a default to take one
        // of: a custom pool in bytes still refuses one.
        assert!(
            over(
                specs(&[("vram", Unit::Bytes, None)]),
                &["--resource", "vram=50%"]
            )
            .is_err()
        );
    }

    #[test]
    fn auto_approve_is_opt_in() {
        assert!(!parse(&["heph"]).auto_approve);
        assert!(parse(&["heph", "--auto-approve"]).auto_approve);
    }
}
