use std::path::Path;

use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::{engine, htaddr, htlabel, htpkg, htquery};
use anyhow::Context;

/// Resolve a CLI target argument into an `Addr`, relative to package `cwp` under
/// workspace `root`.
///
/// First tries to parse `input` as a (possibly relative) target address —
/// `//pkg:name`, `:name`, `./pkg:name`. If that fails, `input` is treated as a
/// path: when it points at an existing file, the file's `fs` target
/// (`//@heph/fs:file@f=<root-relative path>`) is used; otherwise the original
/// parse error is surfaced.
fn resolve_addr_in(input: &str, cwp: &PkgBuf, root: &Path) -> anyhow::Result<Addr> {
    match htaddr::parse_addr_with_base(input, cwp) {
        Ok(addr) => Ok(addr),
        Err(parse_err) => {
            // Not a valid address — fall back to the file-path sugar, but only
            // when it actually names a file on disk. Otherwise the address parse
            // error is the useful one to show.
            if let Ok(rel) = htpkg::join_rel_checked(cwp.as_str(), input)
                && root.join(&rel).is_file()
            {
                return Ok(crate::pluginfs::file_addr(&rel));
            }
            Err(parse_err).with_context(|| format!("parse {input}"))
        }
    }
}

/// `resolve_addr_in` against the current working package and workspace root.
pub fn resolve_addr(input: &str) -> anyhow::Result<Addr> {
    resolve_addr_in(input, &engine::get_cwp()?, &engine::get_root()?)
}

/// Resolve the target selection for the `run`/`query` commands. Exactly one of
/// the query form (`-e '<expr>'`) or the positional form (`<addr>` /
/// `<label> <package>`) must be supplied. Exclusion is expressed inside the
/// query with the `!` operator (e.g. `-e '//... && !//vendor/...'`).
pub fn resolve_matcher(
    query: &Option<String>,
    arg1: &Option<String>,
    arg2: &Option<String>,
    base_pkg: &PkgBuf,
    allow_all: bool,
) -> anyhow::Result<Matcher> {
    if let Some(q) = query {
        if arg1.is_some() {
            anyhow::bail!(
                "cannot combine -e/--query with positional TARGET arguments; use one or the other"
            );
        }
        let matcher =
            htquery::parse(q, base_pkg).with_context(|| format!("parsing query {q:?}"))?;
        // Record the parsed matcher's shape for telemetry from the one place the
        // real parser actually ran — counts only, never the expression text.
        let mut counts = htelemetry::telemetry::QueryExprCounts::default();
        count_matcher(&matcher, &mut counts);
        htelemetry::telemetry::record_query_expr(&counts);
        return Ok(matcher);
    }

    let arg1 = arg1.as_ref().ok_or_else(|| {
        anyhow::anyhow!("missing TARGET_ADDRESS/LABEL argument (or pass a query with -e '<expr>')")
    })?;
    matcher_from_args(arg1, arg2, base_pkg, allow_all)
}

pub fn matcher_from_args(
    arg1: &str,
    arg2: &Option<String>,
    base_pkg: &PkgBuf,
    allow_all: bool,
) -> anyhow::Result<Matcher> {
    if let Some(package_matcher) = &arg2 {
        let label = arg1;

        if label == "all" {
            if !allow_all {
                anyhow::bail!("label `all` not allowed")
            }

            htpkg::parse(package_matcher, base_pkg)
        } else {
            validate_positional_label(label)?;
            Ok(Matcher::And(vec![
                Matcher::Label(label.into()),
                htpkg::parse(package_matcher, base_pkg)?,
            ]))
        }
    } else {
        let addr_str = arg1;
        let addr = htaddr::parse_addr_with_base(addr_str, base_pkg)
            .map_err(|err| annotate_lone_package_matcher(err, addr_str, base_pkg))
            .with_context(|| format!("parse {}", addr_str))?;
        Ok(Matcher::Addr(addr))
    }
}

/// Turn the address parse error for a lone `//pkg/...` into a pointer at `-e`.
///
/// A single positional argument is an *address*, so `heph query //...` — the
/// obvious way to ask for the whole workspace, and what the `query` help itself
/// used to show — failed with a raw parser error about a missing `:name`, with
/// nothing to suggest that the selection belongs behind `-e`. Only annotate
/// when the argument really is a well-formed package matcher; a genuine typo in
/// an address keeps its own error.
fn annotate_lone_package_matcher(
    err: anyhow::Error,
    input: &str,
    base_pkg: &PkgBuf,
) -> anyhow::Error {
    if htpkg::parse(input, base_pkg).is_err() {
        return err;
    }
    err.context(format!(
        "`{input}` is a package matcher, not a target address — the single-argument \
         form takes one address (`//pkg:name`); to select a package use `-e '{input}'`, \
         or name a label first, e.g. `test {input}`"
    ))
}

/// Check the positional `<LABEL> <PACKAGE_MATCHER>` form's first argument
/// against the label grammar.
///
/// The positional form has no delimiters, so before this check any string at
/// all was a "label" — `heph run 'lint && !go-lint' //...` asked for the label
/// literally spelled `lint && !go-lint`, matched nothing, and exited 0, which
/// is indistinguishable from a build that passed. When the argument reads like
/// a query expression, point at `-e`, which is where that syntax belongs.
fn validate_positional_label(label: &str) -> anyhow::Result<()> {
    let Err(err) = htlabel::validate(label) else {
        return Ok(());
    };
    if htlabel::looks_like_query_expr(label) {
        return Err(err).context(
            "the first positional argument is a bare label, not a query expression — \
             for `&&`, `||`, `!` or grouping use `-e`, e.g. \
             -e 'label(lint) && !label(go-lint) && //...'",
        );
    }
    Err(err)
}

/// Tally a parsed query matcher's nodes into telemetry counts. Walks the tree
/// the real parser produced, so the syntax is never re-interpreted; counts only.
fn count_matcher(m: &Matcher, c: &mut htelemetry::telemetry::QueryExprCounts) {
    match m {
        Matcher::Addr(_) => c.addr += 1,
        Matcher::Label(_) => c.label += 1,
        Matcher::Package(_) => c.package += 1,
        Matcher::PackagePrefix(_) => c.package_prefix += 1,
        Matcher::TreeOutputTo(_) => c.tree_output += 1,
        Matcher::Or(terms) => {
            c.or += 1;
            terms.iter().for_each(|t| count_matcher(t, c));
        }
        Matcher::And(terms) => {
            c.and += 1;
            terms.iter().for_each(|t| count_matcher(t, c));
        }
        Matcher::Not(inner) => {
            c.not += 1;
            count_matcher(inner, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr::parse_addr;
    use crate::htmatcher::Matcher;

    /// A tempdir root with `rel` touched as an empty file.
    fn root_with_file(rel: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        dir
    }

    #[test]
    fn existing_bare_file_resolves_to_fs_file_addr() {
        // A bare path is not a valid address, so it falls back to the file
        // sugar — the fs file target keyed by the root-relative path.
        let root = root_with_file("cmd/server/data.txt");
        let addr = resolve_addr_in("data.txt", &PkgBuf::from("cmd/server"), root.path()).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/data.txt"));
    }

    #[test]
    fn bare_subdir_file_resolves_against_package() {
        let root = root_with_file("cmd/server/src/main.rs");
        let addr =
            resolve_addr_in("src/main.rs", &PkgBuf::from("cmd/server"), root.path()).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/src/main.rs"));
    }

    #[test]
    fn unparseable_missing_path_surfaces_parse_error() {
        // Not an address and not a file on disk → the address parse error wins.
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_addr_in("data.txt", &PkgBuf::from("cmd/server"), dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("parse data.txt"), "{err:#}");
    }

    #[test]
    fn dot_slash_path_to_existing_file_uses_fs_target() {
        // `./somefile.txt` is not a valid address (a relative path ref must name
        // a target), so an existing file takes the fs file sugar.
        let root = root_with_file("cmd/server/somefile.txt");
        let addr =
            resolve_addr_in("./somefile.txt", &PkgBuf::from("cmd/server"), root.path()).unwrap();
        assert_eq!(addr, crate::pluginfs::file_addr("cmd/server/somefile.txt"));
    }

    #[test]
    fn dot_slash_relative_target_with_explicit_name() {
        // The explicit `:name` form is a target address, parsed before any disk
        // check.
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_addr_in("./sub:thing", &PkgBuf::from("cmd/server"), dir.path()).unwrap();
        assert_eq!(addr, parse_addr("//cmd/server/sub:thing").unwrap());
    }

    #[test]
    fn plain_addr_is_parsed_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_addr_in("//lib:core", &PkgBuf::from("cmd/server"), dir.path()).unwrap();
        assert_eq!(addr, parse_addr("//lib:core").unwrap());
    }

    #[test]
    fn colon_relative_target_resolves_against_package() {
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_addr_in(":mytarget", &PkgBuf::from("cmd/server"), dir.path()).unwrap();
        assert_eq!(addr, parse_addr("//cmd/server:mytarget").unwrap());
    }

    #[test]
    fn count_matcher_tallies_node_kinds() {
        let pkg = PkgBuf::from("");
        let m = htquery::parse("//a/... && label(foo) && !label(bar)", &pkg).expect("parse");
        let mut c = htelemetry::telemetry::QueryExprCounts::default();
        count_matcher(&m, &mut c);
        assert_eq!(c.label, 2);
        assert_eq!(c.not, 1);
        assert_eq!(c.and, 1, "the && chain is one And node");
        assert_eq!(c.package_prefix, 1, "`//a/...` is a package prefix");
        assert_eq!(c.addr, 0);
    }

    #[test]
    fn positional_addr_returns_addr() {
        let pkg = PkgBuf::from("");
        let m = matcher_from_args("//foo:bar", &None, &pkg, false).unwrap();
        assert!(matches!(m, Matcher::Addr(_)));
    }

    #[test]
    fn colon_name_resolves_against_base_pkg() {
        let pkg = PkgBuf::from("foo/bar");
        let m = matcher_from_args(":build", &None, &pkg, false).unwrap();
        match m {
            Matcher::Addr(addr) => {
                assert_eq!(addr.package.as_str(), "foo/bar");
                assert_eq!(addr.name, "build");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn resolve_matcher_uses_query_when_present() {
        let pkg = PkgBuf::from("");
        let q = Some("//foo/... && !//foo/vendor/...".to_string());
        let m = resolve_matcher(&q, &None, &None, &pkg, false).unwrap();
        match m {
            Matcher::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], Matcher::PackagePrefix(_)));
                assert!(matches!(children[1], Matcher::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn resolve_matcher_falls_back_to_positional() {
        let pkg = PkgBuf::from("");
        let m = resolve_matcher(&None, &Some("//foo:bar".to_string()), &None, &pkg, false).unwrap();
        assert!(matches!(m, Matcher::Addr(_)));
    }

    #[test]
    fn resolve_matcher_rejects_query_with_positional() {
        let pkg = PkgBuf::from("");
        let err = resolve_matcher(
            &Some("//foo".to_string()),
            &Some("//foo:bar".to_string()),
            &None,
            &pkg,
            false,
        )
        .err()
        .expect("expected conflict error");
        assert!(
            format!("{err:#}").contains("cannot combine"),
            "expected conflict message: {err:#}"
        );
    }

    #[test]
    fn positional_label_outside_the_grammar_is_rejected() {
        // The motivating bug: this is a well-formed request for the label
        // literally spelled `lint && !go-lint`, which no target carries — so it
        // used to select nothing and exit 0.
        let pkg = PkgBuf::from("");
        let err = matcher_from_args("lint && !go-lint", &Some("//...".to_string()), &pkg, false)
            .err()
            .expect("expected label validation error");
        let chain = format!("{err:#}");
        assert!(chain.contains("invalid label"), "{chain}");
        assert!(chain.contains("-e"), "expected a pointer to -e: {chain}");
    }

    #[test]
    fn positional_label_error_omits_the_query_hint_for_a_plain_typo() {
        let pkg = PkgBuf::from("");
        let err = matcher_from_args("go:lint", &Some("//...".to_string()), &pkg, false)
            .err()
            .expect("expected label validation error");
        let chain = format!("{err:#}");
        assert!(chain.contains("invalid label"), "{chain}");
        assert!(
            !chain.contains("query expression"),
            "no operators typed, so no -e hint: {chain}"
        );
    }

    #[test]
    fn lone_package_matcher_points_at_the_query_flag() {
        // `heph query //...` — what the query help itself used to advertise. It
        // is a package matcher in the address slot, so it cannot resolve; the
        // error has to say where that selection belongs.
        let pkg = PkgBuf::from("");
        for input in ["//...", "//cmd/...", "//cmd/server", "./sub/..."] {
            let err = matcher_from_args(input, &None, &pkg, true)
                .err()
                .unwrap_or_else(|| panic!("{input:?} unexpectedly resolved as an address"));
            let chain = format!("{err:#}");
            assert!(
                chain.contains("package matcher, not a target address"),
                "{input:?}: {chain}"
            );
            assert!(
                chain.contains(&format!("-e '{input}'")),
                "{input:?} should be quoted back behind -e: {chain}"
            );
        }
    }

    #[test]
    fn malformed_address_keeps_its_own_error() {
        // Not a valid package matcher either, so there is no `-e` form to
        // suggest — the address parse error is the useful one.
        let pkg = PkgBuf::from("");
        let err = matcher_from_args("cmd/server:bin", &None, &pkg, true)
            .err()
            .expect("expected parse error");
        let chain = format!("{err:#}");
        assert!(chain.contains("parse cmd/server:bin"), "{chain}");
        assert!(
            !chain.contains("package matcher"),
            "no package matcher hint for a malformed address: {chain}"
        );
    }

    #[test]
    fn positional_label_within_the_grammar_is_accepted() {
        // Checked directly: the accepting path goes on to resolve the package
        // matcher against the ambient workspace root, which a unit test has no
        // business depending on.
        for l in ["lint", "go-lint", "go_lint2", "Lint"] {
            validate_positional_label(l).unwrap_or_else(|e| panic!("{l:?}: {e:#}"));
        }
    }

    #[test]
    fn resolve_matcher_requires_some_selection() {
        let pkg = PkgBuf::from("");
        assert!(resolve_matcher(&None, &None, &None, &pkg, false).is_err());
    }

    #[test]
    fn invalid_query_surfaces_context() {
        let pkg = PkgBuf::from("");
        let err = resolve_matcher(&Some("bogus(x)".to_string()), &None, &None, &pkg, false)
            .err()
            .expect("expected parse error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("parsing query"),
            "expected 'parsing query' in error chain: {chain}"
        );
    }
}
