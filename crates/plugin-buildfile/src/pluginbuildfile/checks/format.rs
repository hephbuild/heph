//! `build-format-check` and `build-format`: the owned files against
//! `xstarlark-fmt`, through [`hxstarlark_fmt::reformat`], the rule
//! `heph tool build-fmt` applies too. A `heph:fmt skip-file` directive counts
//! as formatted; a file that does not parse fails, named.

use super::report::{Finding, Kind};
use hxstarlark_fmt::{Config, Reformat};

/// `build-format-check` over one owned file: a `format:` finding at the first
/// line that differs from the formatter's output, or `None` when it is
/// formatted. A file that does not parse is an error naming it.
pub(crate) fn check_file(rel: &str, src: &str, config: Config) -> anyhow::Result<Option<Finding>> {
    let formatted = match hxstarlark_fmt::reformat(rel, src, config)? {
        Reformat::Unchanged => return Ok(None),
        Reformat::Changed(formatted) => formatted,
    };
    Ok(Some(Finding {
        file: rel.to_owned(),
        line: first_differing_line(src, &formatted),
        col: 1,
        kind: Kind::Format,
        message: "not formatted; this is the first line that differs from the formatter's output"
            .to_owned(),
    }))
}

/// `build-format-check`'s verdict on one file, for a caller outside the
/// plugin (`heph tool build-fmt`'s agreement test): `None` when the check
/// passes it, else its finding line. A file that does not parse is an error.
pub fn check_verdict(rel: &str, src: &str, indent_size: usize) -> anyhow::Result<Option<String>> {
    Ok(check_file(rel, src, Config { indent_size })?.map(|f| f.to_string()))
}

/// The 1-based line of `src` where `formatted` first differs from it.
fn first_differing_line(src: &str, formatted: &str) -> usize {
    let mut ours = src.split_inclusive('\n');
    let mut theirs = formatted.split_inclusive('\n');
    let mut line = 1;
    while ours.next() == theirs.next() {
        line += 1;
    }
    line
}

/// What `build-format` does with one owned file.
#[derive(Debug)]
pub(crate) enum Fix {
    /// Already formatted, or skipped by the directive: keep its bytes.
    Unchanged,
    /// Write these bytes.
    Rewrite(String),
    /// The file does not parse: keep its bytes, byte-identical, and fail the
    /// run with this error, which names the file.
    Unparseable(anyhow::Error),
}

impl Fix {
    /// The bytes to write for a file whose source is `src`: the formatted
    /// text, or `src` itself, untouched.
    #[cfg(test)]
    pub(crate) fn bytes<'a>(&'a self, src: &'a str) -> &'a str {
        match self {
            Fix::Rewrite(out) => out,
            Fix::Unchanged | Fix::Unparseable(_) => src,
        }
    }
}

/// `build-format` over one owned file.
pub(crate) fn fix_file(rel: &str, src: &str, config: Config) -> Fix {
    match hxstarlark_fmt::reformat(rel, src, config) {
        Ok(Reformat::Unchanged) => Fix::Unchanged,
        Ok(Reformat::Changed(out)) => Fix::Rewrite(out),
        Err(e) => Fix::Unparseable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const CFG: Config = Config { indent_size: 4 };

    /// The format kinds' def hash moves with `indent` and heph's version, and
    /// not with `requireTypeAnnotations`, which only lint reads.
    #[test]
    fn def_hash_inputs() {
        use crate::pluginbuildfile::checks::drivers::parse_spec;
        use crate::pluginbuildfile::checks::targets::{CheckKind, ChecksOptions, format_spec};
        use crate::pluginbuildfile::checks::test_ws::{walk, ws};
        let tmp = ws(&[("pkg/BUILD", "X = 1\n"), ("pkg/more.BUILD", "Y = 1\n")]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        for kind in [CheckKind::FormatCheck, CheckKind::Format] {
            let hash = |indent: usize, require: bool, version: &str| {
                let addr = hmodel::htaddr::Addr::new(
                    hmodel::htpkg::PkgBuf::from("pkg"),
                    kind.name().to_string(),
                    Default::default(),
                );
                let opts = ChecksOptions {
                    require_type_annotations: require,
                    indent,
                };
                parse_spec(format_spec(addr, kind, &graph, opts), version).hash
            };
            let base = hash(4, false, "v1");
            assert_ne!(base, hash(2, false, "v1"), "{kind:?}: indent");
            assert_ne!(base, hash(4, false, "v2"), "{kind:?}: version");
            assert_eq!(base, hash(4, true, "v1"), "{kind:?}: option");
        }
    }

    /// Every formatter fixture's input and expected output.
    fn fixtures() -> Vec<(String, String)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../xstarlark-fmt/testdata");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read testdata") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .to_string();
            let content = std::fs::read_to_string(&path).expect("read fixture");
            // Work-in-progress fixtures, which the golden test skips too.
            if content.contains("# skip") {
                continue;
            }
            let (input, expected) = content.split_once("----\n").expect("separator");
            out.push((format!("{name} input"), input.to_owned()));
            out.push((format!("{name} expected"), expected.to_owned()));
        }
        assert!(
            out.len() > 20,
            "found almost no fixtures in {}",
            dir.display()
        );
        out
    }

    #[test]
    fn idempotent_over_fixtures() {
        for indent_size in [2, 4] {
            let cfg = Config { indent_size };
            for (name, src) in fixtures() {
                let once = fix_file("pkg/BUILD", &src, cfg);
                assert!(!matches!(once, Fix::Unparseable(_)), "{name}: {once:?}");
                let once = once.bytes(&src).to_owned();
                assert!(
                    matches!(fix_file("pkg/BUILD", &once, cfg), Fix::Unchanged),
                    "{name} (indent {indent_size}): formatting twice changed it"
                );
                assert_eq!(
                    check_file("pkg/BUILD", &once, cfg).expect("parses"),
                    None,
                    "{name} (indent {indent_size}): formatted output fails the check"
                );
            }
        }
    }

    /// The finding names the first line that differs, and only that line
    /// (agreement with `heph tool build-fmt` is
    /// `commands::tool::build_fmt::tests::check_matches_build_fmt`).
    #[test]
    fn first_differing_line_reported() {
        assert_eq!(first_differing_line("a\n", "b\n"), 1);
        assert_eq!(first_differing_line("a\nb\n", "a\nc\n"), 2);
        // Only a line missing at the end.
        assert_eq!(first_differing_line("a\n", "a\nb\n"), 2);
        assert_eq!(first_differing_line("a\nb", "a\nb\n"), 2);
        let finding = check_file("pkg/BUILD", "a = 1\nb   = 2\nc   = 3\n", CFG)
            .expect("parses")
            .expect("unformatted");
        assert_eq!(finding.line, 2);
        assert_eq!(
            check_verdict("pkg/BUILD", "a = 1\nb   = 2\n", 4).expect("parses"),
            Some(finding_line(2))
        );
        assert_eq!(
            check_verdict("pkg/BUILD", "a = 1\n", 4).expect("parses"),
            None
        );
    }

    fn finding_line(line: usize) -> String {
        format!(
            "pkg/BUILD:{line}:1: format: not formatted; this is the first line that differs \
             from the formatter's output"
        )
    }

    #[test]
    fn parse_error_fails_untouched() {
        let src = "x = (\n  1,\n";
        let fix = fix_file("pkg/defs.BUILD", src, CFG);
        let Fix::Unparseable(err) = &fix else {
            panic!("an unparseable file is not formatted: {fix:?}");
        };
        assert!(format!("{err:#}").contains("pkg/defs.BUILD"), "{err:#}");
        assert_eq!(fix.bytes(src), src, "left byte-identical");

        let err = check_file("pkg/defs.BUILD", src, CFG).expect_err("fails");
        assert!(format!("{err:#}").contains("pkg/defs.BUILD"), "{err:#}");
    }
}
