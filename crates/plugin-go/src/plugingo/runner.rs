//! Exec-runner plumbing shared by the Go drivers.
//!
//! Every Go driver shells out to a tool — `go list`, `go tool compile`,
//! `gofmt`, `heph-govet` — and each can run that tool inside an environment a
//! runner target describes rather than on the bare host. The wiring is
//! identical in all four, so it lives here rather than four times.
//!
//! The shape mirrors the exec driver exactly, because the correctness argument
//! is the same one:
//!
//! - The runner is a **hash dep** (`hashed: true, runtime: false`). Its hashout
//!   folds into the consumer's `hashin`, so changing the environment re-keys the
//!   compile; and nothing about it enters the sandbox, so the runner target's
//!   own tools and env cannot leak into every Go target's inputs.
//! - The address is **not** in the driver's def hash. `hash_deps` are tracked
//!   through their hashout alone (see
//!   `test_parse_hash_deps_excluded_from_def_hash` in the exec driver), and
//!   following that here means two runner targets describing the same
//!   environment still share cache entries — and that adding the field
//!   invalidates nothing for a workspace that does not use it.
//! - `"local"` is the explicit opt-out, so a package can override a
//!   workspace-wide default without a second knob.

use hmodel::htpkg::PkgBuf;
use hplugin::driver::TargetAddr;
use hplugin::driver::targetdef::{Input, InputMode};
use std::collections::BTreeMap;

/// The `origin_id` a Go driver's runner input carries. Distinct from any dep
/// group, so it is never routed into the tool's inputs.
pub(crate) const RUNNER_ORIGIN: &str = "runner";

/// Resolve a driver's `runner` spec field into the address to run under and the
/// input that puts it in the cache key.
///
/// `None` for an absent field or the literal `"local"`.
pub(crate) fn parse_runner(
    raw: &str,
    pkg: &PkgBuf,
) -> anyhow::Result<(Option<TargetAddr>, Option<Input>)> {
    if raw.is_empty() || raw == "local" {
        return Ok((None, None));
    }
    let target = TargetAddr::parse(raw, pkg).map_err(|e| {
        anyhow::anyhow!(
            "`runner` must be a target address producing a runner.json, or the literal \
             \"local\"; got {raw:?}: {e}"
        )
    })?;
    let input = Input {
        r#ref: target.clone(),
        mode: InputMode::Standard,
        origin_id: RUNNER_ORIGIN.to_string(),
        annotations: BTreeMap::new(),
        hashed: true,
        runtime: false,
    };
    Ok((Some(target), Some(input)))
}

/// Resolve the runner for one target: the spec's own `runner` field when it
/// names one, otherwise the driver-wide default from the plugin's `runner:`
/// option in the config yaml.
///
/// `"local"` means "spawn here" at either level, and a spec that says it beats
/// a default — the field is the escape hatch from a workspace-wide setting,
/// which is what makes turning that setting on safe.
pub(crate) fn parse_runner_with_default(
    raw: &str,
    default: Option<&str>,
    pkg: &PkgBuf,
) -> anyhow::Result<(Option<TargetAddr>, Option<Input>)> {
    let effective = if raw.is_empty() {
        default.unwrap_or_default()
    } else {
        raw
    };
    parse_runner(effective, pkg)
}

/// Read and consume the plugin's `runner` option — the environment every Go
/// tool runs in — from the config-yaml `options:` map.
///
/// Consuming it matters: the go provider's `from_options` rejects keys it does
/// not know, and this one configures the drivers rather than package discovery.
///
/// Split out so the config key itself is under test. A typo here fails nothing
/// — the provider never sees the key, so a misspelling would leave the option
/// silently inert and every Go tool on the host while the config says
/// otherwise.
pub fn take_runner_option(
    options: &mut hplugin::config::Options,
) -> anyhow::Result<Option<String>> {
    let runner = hplugin::config::decode_opt::<String>(options, "go", "runner")?
        .filter(|r| !r.is_empty());
    options.remove("runner");
    Ok(runner)
}

/// The runner to spawn this driver's tool under.
pub(crate) fn runner_ref<'a>(
    request_id: &'a str,
    runner: Option<&'a TargetAddr>,
) -> hexecrunner::RunnerRef<'a> {
    match runner {
        Some(t) => hexecrunner::RunnerRef::target(request_id, &t.r#ref),
        None => hexecrunner::RunnerRef::local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg() -> PkgBuf {
        PkgBuf::from("some/pkg")
    }

    #[test]
    fn absent_and_local_both_mean_no_runner() {
        for raw in ["", "local"] {
            let (target, input) = parse_runner(raw, &pkg()).expect("parse");
            assert!(target.is_none(), "{raw:?}");
            assert!(input.is_none(), "{raw:?}");
        }
    }

    /// hashed so it keys the cache; not runtime so it never reaches the sandbox
    /// and its transitives never merge into the Go target's.
    #[test]
    fn a_runner_is_a_hash_dep() {
        let (target, input) = parse_runner("//tools/devenv:runner", &pkg()).expect("parse");
        assert_eq!(
            target.expect("target").r#ref.format(),
            "//tools/devenv:runner"
        );
        let input = input.expect("input");
        assert!(input.hashed);
        assert!(!input.runtime);
        assert_eq!(input.origin_id, RUNNER_ORIGIN);
    }

    /// Relative to the target's own package, like every other address in a
    /// driver spec.
    #[test]
    fn a_relative_address_resolves_against_the_package() {
        let (target, _) = parse_runner(":runner", &pkg()).expect("parse");
        assert_eq!(target.expect("target").r#ref.format(), "//some/pkg:runner");
    }

    /// The driver-wide default applies only where the spec is silent.
    #[test]
    fn the_default_fills_in_for_a_spec_that_names_no_runner() {
        let (target, input) =
            parse_runner_with_default("", Some("//tools/devenv:runner"), &pkg()).expect("parse");
        assert_eq!(
            target.expect("target").r#ref.format(),
            "//tools/devenv:runner"
        );
        assert!(input.expect("input").hashed);
    }

    /// The spec wins over the default, so a hand-written Go target can name a
    /// different environment than the workspace-wide one.
    #[test]
    fn a_spec_runner_beats_the_default() {
        let (target, _) =
            parse_runner_with_default(":own", Some("//tools/devenv:runner"), &pkg()).expect("parse");
        assert_eq!(target.expect("target").r#ref.format(), "//some/pkg:own");
    }

    /// **The escape hatch.** Without this a workspace-wide default would be
    /// unopt-out-able for the one target that must not have it — which is what
    /// makes turning the default on a safe thing to do.
    #[test]
    fn a_spec_saying_local_escapes_the_default() {
        let (target, input) =
            parse_runner_with_default("local", Some("//tools/devenv:runner"), &pkg()).expect("parse");
        assert!(target.is_none());
        assert!(input.is_none());
    }

    /// A default of `"local"` (or absent) leaves everything on the host, so the
    /// option can be set to the opt-out value rather than deleted.
    #[test]
    fn a_local_or_absent_default_means_no_runner() {
        for default in [None, Some("local"), Some("")] {
            let (target, input) = parse_runner_with_default("", default, &pkg()).expect("parse");
            assert!(target.is_none(), "{default:?}");
            assert!(input.is_none(), "{default:?}");
        }
    }

    fn opts(pairs: &[(&str, &str)]) -> hplugin::config::Options {
        let mut o = hplugin::config::Options::new();
        for (k, v) in pairs {
            o.insert((*k).to_string(), serde_yaml::Value::String((*v).to_string()));
        }
        o
    }

    /// The config-yaml key the drivers are actually configured by.
    #[test]
    fn the_runner_option_is_read_and_consumed() {
        let mut o = opts(&[("gotool", "host"), ("runner", "//tools/devenv:runner")]);
        assert_eq!(
            take_runner_option(&mut o).expect("decode").as_deref(),
            Some("//tools/devenv:runner")
        );
        assert!(
            !o.contains_key("runner"),
            "must be consumed — the go provider rejects keys that are not its own"
        );
        assert!(o.contains_key("gotool"), "unrelated options are untouched");
    }

    /// Absent and empty both mean "spawn on the host", so the option can be
    /// blanked rather than deleted.
    #[test]
    fn an_absent_or_empty_option_means_no_runner() {
        assert_eq!(take_runner_option(&mut opts(&[])).expect("decode"), None);
        assert_eq!(
            take_runner_option(&mut opts(&[("runner", "")])).expect("decode"),
            None
        );
    }

    /// A bare word that is not the reserved `local` must say what the field
    /// takes, rather than failing later as "target not found".
    #[test]
    fn a_bare_word_is_rejected_with_the_shape() {
        let err = match parse_runner("locl", &pkg()) {
            Ok(_) => panic!("a bare word that is not `local` must be rejected"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("target address"), "{msg}");
        assert!(msg.contains("local"), "{msg}");
    }
}
