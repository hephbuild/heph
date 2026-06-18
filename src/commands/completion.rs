use std::ffi::OsStr;
use std::sync::Arc;

use clap_complete::engine::CompletionCandidate;
use futures::TryStreamExt;

use crate::commands::bootstrap;
use crate::engine::request_state::RequestState;
use crate::engine::{Engine, get_cwp};
use crate::htaddr::parse_addr_with_base;
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
/// `base` is the current package, used to resolve the relative forms.
/// Candidates are emitted in the same style the user is typing, mirroring the
/// address forms `parse_addr_with_base` accepts: absolute `//pkg:name`,
/// `:name`, `./sub:name`, `../sib:name`, and bare `name` (a target in `base`).
async fn addr_candidates(
    engine: &Arc<Engine>,
    base: &PkgBuf,
    partial: &str,
) -> anyhow::Result<Vec<String>> {
    let rs = engine.new_state();

    // Absolute "//pkg" / "//pkg:name".
    if let Some(body) = partial.strip_prefix("//") {
        if let Some((pkg_part, name_prefix)) = body.rsplit_once(':') {
            // Targets in that exact package. Matching on `Package` yields
            // `MatchYes` without a def eval, so this costs one BUILD eval.
            let pkg = PkgBuf::from(pkg_part);
            let names = targets_in(engine, &rs, &pkg).await?;
            return Ok(filter_map_dedup(names, name_prefix, |n| {
                format!("//{pkg}:{n}")
            }));
        }
        // "//pkg" prefix — keep packages whose path starts with what's typed.
        // A plain string prefix (not `PackagePrefix`, which narrows
        // component-wise) is what the user expects mid-word: typing `go` must
        // also surface a sibling `group`, not just the `go` subtree. clap does
        // not post-filter `ArgValueCompleter` candidates, so we filter here.
        // The trailing colon cues the shell into completing target names next.
        let pkgs = all_packages(engine, &rs).await?;
        return Ok(filter_map_dedup(pkgs, body, |p| format!("//{p}:")));
    }

    // Relative ":name" — a target in the current package.
    if let Some(name_prefix) = partial.strip_prefix(':') {
        let names = targets_in(engine, &rs, base).await?;
        return Ok(filter_map_dedup(names, name_prefix, |n| format!(":{n}")));
    }

    // Relative "./sub" / "../sib" path forms.
    if partial.starts_with("./") || partial.starts_with("../") {
        if let Some((prefix, name_prefix)) = partial.split_once(':') {
            // Package fully typed — complete the target name. Resolve the
            // relative package through the real parser, then re-emit with the
            // exact prefix the user typed.
            let Ok(probe) = parse_addr_with_base(&format!("{prefix}:__heph_probe__"), base) else {
                return Ok(Vec::new());
            };
            let pkg = probe.package.clone();
            let names = targets_in(engine, &rs, &pkg).await?;
            return Ok(filter_map_dedup(names, name_prefix, |n| {
                format!("{prefix}:{n}")
            }));
        }
        // Still typing the package path — render each package relative to
        // `base` and keep the ones that round-trip back to the same package.
        let pkgs = all_packages(engine, &rs).await?;
        let mut out = Vec::new();
        for p in pkgs {
            let p = PkgBuf::from(p);
            if let Some(rel) = render_relative_pkg(base, &p)
                && rel.starts_with(partial)
                && round_trips(&rel, base, &p)
            {
                out.push(format!("{rel}:"));
            }
        }
        return Ok(dedup(out));
    }

    // Bare "name" — a target in the current package. A path separator with
    // none of the markers above is not a valid relative form, so skip it.
    if !partial.contains('/') {
        let names = targets_in(engine, &rs, base).await?;
        return Ok(dedup(names.into_iter().filter(|n| n.starts_with(partial))));
    }

    Ok(Vec::new())
}

/// List every package the providers know about (single cached filesystem walk).
async fn all_packages(engine: &Arc<Engine>, rs: &Arc<RequestState>) -> anyhow::Result<Vec<String>> {
    let m = Matcher::PackagePrefix(PkgBuf::from(""));
    let it = engine.packages(&m, rs).await?;
    let mut v = Vec::new();
    for p in it {
        v.push(p?);
    }
    Ok(v)
}

/// Render absolute package `p` as the relative path string that resolves back
/// to it from `base` (e.g. `./sub`, `../sib`, `../`). Returns `None` for the
/// current package itself — that is the `:name` form, not a path.
fn render_relative_pkg(base: &PkgBuf, p: &PkgBuf) -> Option<String> {
    let b: Vec<&str> = base.as_str().split('/').filter(|s| !s.is_empty()).collect();
    let t: Vec<&str> = p.as_str().split('/').filter(|s| !s.is_empty()).collect();

    let common = b.iter().zip(t.iter()).take_while(|(x, y)| x == y).count();
    let ups = b.len() - common;
    let rest = t.get(common..).map(|s| s.join("/")).unwrap_or_default();

    if ups == 0 {
        if rest.is_empty() {
            return None; // p == base
        }
        return Some(format!("./{rest}"));
    }
    let mut s = "../".repeat(ups);
    s.push_str(&rest);
    Some(s)
}

/// Confirm a rendered relative path parses back to the intended package, so a
/// faulty render is dropped rather than offered as a broken completion.
fn round_trips(rel: &str, base: &PkgBuf, pkg: &PkgBuf) -> bool {
    parse_addr_with_base(&format!("{rel}:__heph_probe__"), base)
        .map(|a| a.package == *pkg)
        .unwrap_or(false)
}

/// Filter `items` by `prefix`, render each survivor, then sort + dedup.
fn filter_map_dedup(
    items: Vec<String>,
    prefix: &str,
    render: impl Fn(&str) -> String,
) -> Vec<String> {
    dedup(
        items
            .into_iter()
            .filter(|s| s.starts_with(prefix))
            .map(|s| render(&s)),
    )
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

        e.register_provider_factory("buildfile", |init, opts| {
            Ok(Box::new(pluginbuildfile::Provider::from_options(
                init.root.to_path_buf(),
                &init.skip_dirs,
                &init.skip_globs,
                opts,
            )?))
        })
        .expect("register buildfile");

        e.apply_builtin("buildfile", &Default::default())
            .expect("apply buildfile");

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

    /// root + foo (bar/baz/qux) + foo/sub (subtgt) + bar (bartgt).
    fn fixture_nested(root: &std::path::Path) {
        fixture(root);
        let sub = root.join("foo").join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("BUILD"),
            "target(name = \"subtgt\", driver = \"d\")\n",
        )
        .unwrap();
        let bar = root.join("bar");
        fs::create_dir_all(&bar).unwrap();
        fs::write(
            bar.join("BUILD"),
            "target(name = \"bartgt\", driver = \"d\")\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn completes_bare_target_in_base_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from("foo"), "ba")
            .await
            .expect("candidates");

        assert_eq!(got, vec!["bar".to_string(), "baz".to_string()]);
    }

    #[tokio::test]
    async fn completes_descendant_package_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture_nested(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from("foo"), "./")
            .await
            .expect("candidates");

        assert_eq!(got, vec!["./sub:".to_string()]);
    }

    #[tokio::test]
    async fn completes_relative_target_in_descendant_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture_nested(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from("foo"), "./sub:")
            .await
            .expect("candidates");

        assert_eq!(got, vec!["./sub:subtgt".to_string()]);
    }

    #[tokio::test]
    async fn completes_sibling_package_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture_nested(dir.path());
        let engine = engine_at(dir.path().to_path_buf());

        let got = addr_candidates(&engine, &PkgBuf::from("foo"), "../bar")
            .await
            .expect("candidates");

        assert_eq!(got, vec!["../bar:".to_string()]);
    }
}
