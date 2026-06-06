use std::ffi::OsStr;
use std::sync::Arc;

use clap_complete::engine::CompletionCandidate;
use futures::TryStreamExt;

use crate::commands::bootstrap;
use crate::engine::request_state::RequestState;
use crate::engine::{Engine, get_cwp};
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;

/// clap dynamic-completion entry point for target-address arguments.
///
/// Spins up a short-lived engine, asks it for the packages / targets matching
/// what the user has typed so far, and turns them into completion candidates.
/// Any failure — not inside a heph workspace, a cancelled walk, an IO error —
/// yields no candidates rather than surfacing an error to the shell.
pub fn complete_target_addr(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(partial) = current.to_str() else {
        return Vec::new();
    };

    candidates(partial)
        .unwrap_or_default()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

/// Build the engine and drive the async listing. Kept separate from
/// [`complete_target_addr`] so the error path is a single `?`-chain.
fn candidates(partial: &str) -> anyhow::Result<Vec<String>> {
    let base = get_cwp()?;
    bootstrap::block_on(async move {
        // `new_engine` spawns tokio tasks, so it must run inside the runtime.
        let (engine, _shutdown) = bootstrap::new_engine()?;
        addr_candidates(&engine, &base, partial).await
    })?
}

/// Core completion logic, factored out so it can be unit-tested against an
/// in-memory engine without a real working directory or shell.
///
/// `base` is the current package, used to resolve the relative `:name` form.
async fn addr_candidates(
    engine: &Arc<Engine>,
    base: &PkgBuf,
    partial: &str,
) -> anyhow::Result<Vec<String>> {
    let rs = engine.new_state();

    // Relative ":name" — complete targets in the current package, preserving
    // the relative form the user is typing.
    if let Some(name_prefix) = partial.strip_prefix(':') {
        let names = targets_in(engine, &rs, base).await?;
        return Ok(dedup(
            names
                .into_iter()
                .filter(|n| n.starts_with(name_prefix))
                .map(|n| format!(":{n}")),
        ));
    }

    // Absolute address; the leading "//" may be only partially typed.
    let body = partial.strip_prefix("//").unwrap_or(partial);

    if let Some((pkg_part, name_prefix)) = body.rsplit_once(':') {
        // "//pkg:name" — list targets in that exact package. Matching on
        // `Package` yields `MatchYes` without a def eval, so this only costs
        // one BUILD evaluation.
        let pkg = PkgBuf::from(pkg_part);
        let names = targets_in(engine, &rs, &pkg).await?;
        return Ok(dedup(
            names
                .into_iter()
                .filter(|n| n.starts_with(name_prefix))
                .map(|n| format!("//{pkg}:{n}")),
        ));
    }

    // "//pkg" prefix — list every package and keep those whose path starts
    // with what's typed. A plain string prefix (not `PackagePrefix`, which
    // narrows component-wise) is what the user expects mid-word: typing `go`
    // must also surface a sibling `group`, not just the `go` subtree. clap
    // does not post-filter `ArgValueCompleter` candidates, so we filter here.
    // The trailing colon cues the shell into the next round of target names.
    let m = Matcher::PackagePrefix(PkgBuf::from(""));
    let pkgs = engine.packages(&m, &rs).await?;
    let mut out = Vec::new();
    for p in pkgs {
        let p = p?;
        if p.starts_with(body) {
            out.push(format!("//{p}:"));
        }
    }
    Ok(dedup(out))
}

/// List the target names defined in a single package.
async fn targets_in(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    pkg: &PkgBuf,
) -> anyhow::Result<Vec<String>> {
    let m = Matcher::Package(pkg.clone());
    let stream = engine.clone().query(rs.clone(), &m);
    tokio::pin!(stream);

    let mut names = Vec::new();
    while let Some(addr) = stream.try_next().await? {
        names.push(addr.name.clone());
    }
    Ok(names)
}

/// Sort + dedup candidates: multiple providers can surface the same package or
/// target name, and shells show duplicates verbatim.
fn dedup(it: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = it.into_iter().collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Config, Engine};
    use crate::pluginbuildfile;
    use std::fs;
    use std::sync::Arc;

    /// Build an engine with the `buildfile` provider rooted at `root`.
    fn engine_at(root: std::path::PathBuf) -> Arc<Engine> {
        let home_dir = root.join(".heph3");
        let mut e = Engine::new(Config {
            root,
            home_dir,
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");

        e.register_provider_factory("buildfile", |root, skip_dirs, opts| {
            Ok(Box::new(pluginbuildfile::Provider::from_options(
                root.to_path_buf(),
                skip_dirs,
                opts,
            )?))
        })
        .expect("register buildfile");

        let file: crate::engine::config_file::ConfigFile =
            serde_yaml::from_str("providers:\n  - name: buildfile\n").expect("yaml");
        e.apply_config(&file.providers, &file.drivers)
            .expect("apply_config");

        Arc::new(e)
    }

    /// root BUILD (package "") + foo/BUILD with targets bar, baz, qux.
    fn fixture(root: &std::path::Path) {
        fs::write(
            root.join("BUILD"),
            "target(name = \"root_tgt\", driver = \"d\")\n",
        )
        .unwrap();
        let foo = root.join("foo");
        fs::create_dir_all(&foo).unwrap();
        fs::write(
            foo.join("BUILD"),
            "target(name = \"bar\", driver = \"d\")\n\
             target(name = \"baz\", driver = \"d\")\n\
             target(name = \"qux\", driver = \"d\")\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn completes_packages_with_trailing_colon() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from(""), "//")
            .await
            .expect("candidates");

        assert!(got.contains(&"//foo:".to_string()), "{got:?}");
        assert!(got.contains(&"//:".to_string()), "{got:?}");
    }

    #[tokio::test]
    async fn filters_packages_by_string_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from(""), "//fo")
            .await
            .expect("candidates");

        assert_eq!(got, vec!["//foo:".to_string()]);
    }

    #[tokio::test]
    async fn completes_targets_in_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from(""), "//foo:")
            .await
            .expect("candidates");

        assert_eq!(
            got,
            vec![
                "//foo:bar".to_string(),
                "//foo:baz".to_string(),
                "//foo:qux".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn filters_targets_by_name_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from(""), "//foo:ba")
            .await
            .expect("candidates");

        assert_eq!(got, vec!["//foo:bar".to_string(), "//foo:baz".to_string()]);
    }

    #[tokio::test]
    async fn completes_relative_targets_in_base_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from("foo"), ":b")
            .await
            .expect("candidates");

        assert_eq!(got, vec![":bar".to_string(), ":baz".to_string()]);
    }
}
