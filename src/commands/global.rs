use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use hcore::capacity::{Capacity, Zero};
use hplugin::provider::PoolName;

/// The prefix of the per-pool capacity variables in heph's own environment:
/// `HEPH_RESOURCE_GPU=2` is `--resource gpu=2`.
pub const RESOURCE_ENV_PREFIX: &str = "HEPH_RESOURCE_";

/// Pool capacities keyed by pool, as one layer of the precedence
/// (config, environment, flags) writes them.
pub type ResourceSpecs = BTreeMap<PoolName, Capacity>;

/// One `--resource <name>=<n>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceOverride {
    pub pool: PoolName,
    pub capacity: Capacity,
}

/// Parse `-j/--jobs`: `--resource cpu=<n>`, so whole CPUs or a percentage.
fn parse_jobs(s: &str) -> Result<Capacity, String> {
    Capacity::parse(PoolName::CPU, s, Zero::Refused).map_err(|e| format!("{e:#}"))
}

/// Parse `--resource <name>=<n>`. A run's override gives the pool a number,
/// so `0` is refused (a config may declare `gpu: 0`; the run sets it).
fn parse_resource_override(s: &str) -> Result<ResourceOverride, String> {
    let (name, value) = s
        .split_once('=')
        .ok_or_else(|| format!("{s:?} is not <name>=<n>: write e.g. gpu=2 or cpu=50%"))?;
    let pool = PoolName::try_from(name).map_err(|e| format!("{e:#}"))?;
    let capacity = Capacity::parse(pool.as_str(), value, Zero::Refused)
        .map_err(|e| format!("pool {pool}: {e:#}", pool = pool.as_str()))?;
    Ok(ResourceOverride { pool, capacity })
}

/// The `HEPH_RESOURCE_<NAME>=<n>` overrides in `vars` (heph's environment).
/// `<NAME>` is lowercased and must then be a pool name; the value parses as a
/// `--resource` value does. Any malformed one is an error naming the
/// variable, never a silently ignored setting.
pub fn env_resource_overrides(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> anyhow::Result<ResourceSpecs> {
    let mut out = ResourceSpecs::new();
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
        let capacity = Capacity::parse(pool.as_str(), value, Zero::Refused)
            .with_context(|| format!("{key}={value}"))?;
        if out.insert(pool, capacity).is_some() {
            // Only reachable through case: `HEPH_RESOURCE_GPU` and `..._gpu`.
            anyhow::bail!("{key}: pool {suffix} is set by two variables");
        }
    }
    Ok(out)
}

/// Lay the run's overrides over the config's capacities: flag, then
/// environment, then config. A pool in none of them keeps the engine's
/// default.
pub fn layer_resources(config: &mut ResourceSpecs, env: ResourceSpecs, flags: &ResourceSpecs) {
    config.extend(env);
    config.extend(flags.iter().map(|(p, c)| (p.clone(), *c)));
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
    /// The `cpu` pool's capacity for this run: whole CPUs, or a percentage of
    /// the default (`-j 50%`). Each target reserves what its `resources`
    /// declares (one CPU unless it says otherwise), and targets wait until
    /// their reservation fits. The same as `--resource cpu=<N>`; giving both
    /// is an error. Default: the config's `resources.cpu`, else the
    /// engine's parallelism.
    #[arg(
        short = 'j',
        long = "jobs",
        value_name = "N",
        value_parser = parse_jobs,
        global = true
    )]
    pub jobs: Option<Capacity>,
    /// Set a resource pool's capacity for this run: `<name>=<n>`. Repeatable
    ///
    /// `cpu` in CPUs, `memory` as a size (`24G`), a custom pool as a count;
    /// `cpu` and `memory` also take a percentage of their default
    /// (`memory=75%`). At least 1. Overrides `HEPH_RESOURCE_<NAME>`, which
    /// overrides `resources.<name>` in `.hephconfig`. A pool may be given
    /// once per command line.
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
    pub fn resource_overrides(&self) -> anyhow::Result<ResourceSpecs> {
        let jobs = self.jobs.map(|capacity| ResourceOverride {
            pool: PoolName::cpu(),
            capacity,
        });
        let mut out = ResourceSpecs::new();
        for o in jobs.iter().chain(&self.resources) {
            if out.insert(o.pool.clone(), o.capacity).is_some() {
                let pool = o.pool.as_str();
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
    fn overrides(args: &[&str]) -> anyhow::Result<ResourceSpecs> {
        let argv = std::iter::once("heph").chain(args.iter().copied());
        TestCli::try_parse_from(argv)?.global.resource_overrides()
    }

    fn env(vars: &[(&str, &str)]) -> anyhow::Result<ResourceSpecs> {
        env_resource_overrides(vars.iter().map(|(k, v)| (k.into(), v.into())))
    }

    fn pool(s: &str) -> PoolName {
        PoolName::try_from(s).unwrap()
    }

    fn specs(rows: &[(&str, Capacity)]) -> ResourceSpecs {
        rows.iter().map(|(p, c)| (pool(p), *c)).collect()
    }

    const CPU: u64 = hcore::capacity::MICROS_PER_CPU;

    #[test]
    fn resource_override_parse_table() {
        use Capacity::Amount;
        // Accepted: the pool's unit, as the config writes it.
        for (args, want) in [
            (&["-j", "4"][..], specs(&[("cpu", Amount(4 * CPU))])),
            (&["--jobs=4"], specs(&[("cpu", Amount(4 * CPU))])),
            (&["--resource", "gpu=2"], specs(&[("gpu", Amount(2))])),
            (
                &["--resource", "memory=24G"],
                specs(&[("memory", Amount(24 << 30))]),
            ),
            // The bound: whole CPUs up to u32::MAX.
            (
                &["-j", "4294967295"],
                specs(&[("cpu", Amount(u64::from(u32::MAX) * CPU))]),
            ),
            (
                &["--resource", "gpu=1", "--resource", "docker=3"],
                specs(&[("gpu", Amount(1)), ("docker", Amount(3))]),
            ),
            (&[], ResourceSpecs::new()),
        ] {
            assert_eq!(overrides(args).unwrap(), want, "{args:?}");
        }
        // Refused: a run's override gives the pool a number, at least 1.
        for args in [
            &["-j", "0"][..],
            &["--resource", "gpu=0"],
            &["--resource", "cpu=0"],
            &["-j", "-1"],
            &["--resource", "gpu=-1"],
            &["-j", "4294967296"],
            &["--resource", "gpu=4294967296"],
            &["-j", "1.5"],
            &["-j", "abc"],
            &["--resource", "gpu=abc"],
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

        // The environment: `<NAME>` lowercased, the value as `--resource`'s.
        assert_eq!(
            env(&[
                ("HEPH_RESOURCE_GPU", "2"),
                ("HEPH_RESOURCE_CPU", "3"),
                ("HEPH_RESOURCES", "nope"),
                ("PATH", "/bin"),
            ])
            .unwrap(),
            specs(&[("gpu", Amount(2)), ("cpu", Amount(3 * CPU))])
        );
        for vars in [
            &[("HEPH_RESOURCE_GPU", "0")][..],
            &[("HEPH_RESOURCE_GPU", "-1")],
            &[("HEPH_RESOURCE_GPU", "abc")],
            &[("HEPH_RESOURCE_GPU", "")],
            &[("HEPH_RESOURCE_", "1")],
            &[("HEPH_RESOURCE_A-B", "1")],
            &[("HEPH_RESOURCE_1GPU", "1")],
            &[("HEPH_RESOURCE_GPU", "1"), ("HEPH_RESOURCE_gpu", "2")],
        ] {
            assert!(env(vars).is_err(), "{vars:?} was accepted");
        }
        // The error names the variable, not just the value.
        let err = format!("{:#}", env(&[("HEPH_RESOURCE_GPU", "0")]).unwrap_err());
        assert!(err.contains("HEPH_RESOURCE_GPU"), "{err}");
    }

    #[test]
    fn jobs_is_sugar_for_resource_cpu() {
        for n in ["4", "50%"] {
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
                ("cpu", Capacity::Amount(2 * CPU)),
                ("gpu", Capacity::Amount(1))
            ])
        );
    }

    #[test]
    fn override_precedence_is_flag_env_config_default() {
        use Capacity::Amount;
        let mut layered = specs(&[
            ("cpu", Amount(2 * CPU)),
            ("gpu", Amount(0)),
            ("docker", Amount(3)),
        ]);
        let env = env(&[("HEPH_RESOURCE_CPU", "4"), ("HEPH_RESOURCE_GPU", "5")]).unwrap();
        let flags = overrides(&["-j", "6"]).unwrap();
        layer_resources(&mut layered, env, &flags);
        let got: BTreeMap<String, u64> =
            crate::engine::resource_pool::resolve_capacities(&layered, 8, || 1 << 30)
                .into_iter()
                .map(|(p, n)| (p.as_str().to_string(), n))
                .collect();
        let want: BTreeMap<String, u64> = [
            ("cpu", 6 * CPU),    // flag over env over config
            ("gpu", 5),          // env over config's 0
            ("docker", 3),       // config
            ("memory", 1 << 30), // engine default
        ]
        .into_iter()
        .map(|(p, n)| (p.to_string(), n))
        .collect();
        assert_eq!(got, want);

        // Nothing written anywhere: the engine's defaults.
        let mut none = ResourceSpecs::new();
        layer_resources(&mut none, ResourceSpecs::new(), &ResourceSpecs::new());
        let got = crate::engine::resource_pool::resolve_capacities(&none, 8, || 7);
        assert_eq!(
            got,
            vec![(PoolName::cpu(), 8 * CPU), (PoolName::memory(), 7)]
        );
    }

    #[test]
    fn percent_capacity_parse_table() {
        let resolve = |args: &[&str], host_memory: u64| {
            let flags = overrides(args).unwrap();
            crate::engine::resource_pool::resolve_capacities(&flags, 8, || host_memory)
                .into_iter()
                .map(|(p, n)| (p.as_str().to_string(), n))
                .collect::<BTreeMap<_, _>>()
        };
        // `-j 50%` on parallelism 8: 4 CPUs.
        assert_eq!(resolve(&["-j", "50%"], 1)["cpu"], 4 * CPU);
        // memory floors to bytes.
        assert_eq!(resolve(&["--resource", "memory=75%"], 3)["memory"], 2);
        // Over 100% is an overcommit by choice.
        assert_eq!(resolve(&["-j", "150%"], 1)["cpu"], 12 * CPU);
        assert_eq!(
            env(&[("HEPH_RESOURCE_CPU", "50%")]).unwrap(),
            specs(&[("cpu", Capacity::Percent(50))])
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
        assert!(env(&[("HEPH_RESOURCE_GPU", "50%")]).is_err());
    }

    #[test]
    fn auto_approve_is_opt_in() {
        assert!(!parse(&["heph"]).auto_approve);
        assert!(parse(&["heph", "--auto-approve"]).auto_approve);
    }
}
