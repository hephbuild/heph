/// Port of the Go `generate_test_main.go` reference implementation.
///
/// Parses Go test source files using a simple line-by-line approach (no full AST),
/// then generates the `testmain.go` bootstrap file that the Go test framework requires.
use anyhow::Context;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFunc {
    pub package: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    pub package: String,
    pub name: String,
    pub output: String,
    pub unordered: bool,
}

/// Metadata extracted from test source files.
#[derive(Debug, Default)]
pub struct Analysis {
    pub tests: Vec<TestFunc>,
    pub benchmarks: Vec<TestFunc>,
    pub fuzz_targets: Vec<TestFunc>,
    pub examples: Vec<Example>,
    pub test_main: Option<TestFunc>,
    pub import_path: String,
    pub import_test: bool,
    pub import_xtest: bool,
    pub need_test: bool,
    pub need_xtest: bool,
    /// Always true for modern Go (≥1.18).
    pub is_go1_18: bool,
}

#[derive(Debug, Default)]
struct PartialAnalysis {
    tests: Vec<TestFunc>,
    benchmarks: Vec<TestFunc>,
    fuzz_targets: Vec<TestFunc>,
    // examples are omitted (require full AST)
    test_main: Option<TestFunc>,
    need_test: bool,
    need_xtest: bool,
}

/// Parse Go source for test functions using a line-by-line heuristic.
///
/// Matches lines that begin with `func ` at column 0 (no leading whitespace) and
/// inspects the function name and parameter type. Correctly ignores method
/// declarations (receiver between `func` and the name) and nested/indented funcs.
///
/// Streams lines from `reader` so file content never has to be held in full.
fn process_reader<R: BufRead>(pkg_label: &str, reader: R) -> std::io::Result<PartialAnalysis> {
    let mut result = PartialAnalysis::default();

    // Comment/raw-string state carried across lines: a `/* … */` block or a
    // backtick raw string can span multiple lines, and a `func Test…` sitting
    // inside either must NOT be scanned as a real test (the bug: a commented-out
    // `func TestDiblManual` at column 0 got pulled into testmain.go).
    let mut state = ScanState::default();
    for line in reader.lines() {
        let line = line?;
        let code = blank_comments(&line, &mut state);
        process_line(pkg_label, &code, &mut result);
    }

    let has_findings = !result.tests.is_empty()
        || !result.benchmarks.is_empty()
        || !result.fuzz_targets.is_empty()
        || result.test_main.is_some();
    if has_findings {
        match pkg_label {
            "_test" => result.need_test = true,
            "_xtest" => result.need_xtest = true,
            _ => {}
        }
    }
    Ok(result)
}

/// Multi-line scanner state: inside a `/* … */` block comment, or inside a
/// backtick raw string — both can span lines.
#[derive(Default)]
struct ScanState {
    in_block_comment: bool,
    in_raw_string: bool,
}

/// Return `line` with comment bytes blanked to spaces (column positions
/// preserved so the column-0 `func ` check still holds), tracking block-comment
/// and raw-string state across lines via `state`. Handles `//` line comments,
/// `/* … */` block comments, `"…"`/`'…'` (with `\` escapes) and backtick raw
/// strings so a `/*` or `func` *inside* a literal is not mistaken for code.
/// Go block comments do not nest. Operates on bytes; only ASCII delimiters are
/// inspected, non-ASCII bytes inside literals are preserved.
fn blank_comments(line: &str, state: &mut ScanState) -> String {
    // Bounds-checked accessor: blank one byte to a space if in range.
    fn blank(buf: &mut [u8], idx: usize) {
        if let Some(b) = buf.get_mut(idx) {
            *b = b' ';
        }
    }

    let mut out = line.as_bytes().to_vec();
    let mut i = 0;
    while let Some(&c) = out.get(i) {
        let next = out.get(i + 1).copied();
        if state.in_raw_string {
            // Raw-string content (and its closing backtick) is a literal, not
            // code — blank it so a `func Test…` on a line inside it is not scanned.
            if c == b'`' {
                state.in_raw_string = false;
            }
            blank(&mut out, i);
            i += 1;
        } else if state.in_block_comment {
            if c == b'*' && next == Some(b'/') {
                state.in_block_comment = false;
                blank(&mut out, i);
                blank(&mut out, i + 1);
                i += 2;
            } else {
                blank(&mut out, i);
                i += 1;
            }
        } else if c == b'/' && next == Some(b'/') {
            // Line comment: blank the rest of the line.
            for b in out.iter_mut().skip(i) {
                *b = b' ';
            }
            break;
        } else if c == b'/' && next == Some(b'*') {
            state.in_block_comment = true;
            blank(&mut out, i);
            blank(&mut out, i + 1);
            i += 2;
        } else if c == b'`' {
            state.in_raw_string = true;
            blank(&mut out, i);
            i += 1;
        } else if c == b'"' || c == b'\'' {
            // Interpreted string / rune: skip to the matching quote, honoring
            // `\` escapes. Go does not allow a newline inside these, so any
            // unterminated quote ends at the line break. Content is code (kept).
            let quote = c;
            i += 1;
            while let Some(&q) = out.get(i) {
                if q == b'\\' {
                    i += 2;
                    continue;
                }
                i += 1;
                if q == quote {
                    break;
                }
            }
        } else {
            i += 1;
        }
    }
    // Lossy is safe: only ASCII delimiter bytes were rewritten to spaces; any
    // multi-byte UTF-8 sequences inside literals were stepped over intact.
    String::from_utf8_lossy(&out).into_owned()
}

fn process_line(pkg_label: &str, line: &str, result: &mut PartialAnalysis) {
    let Some(rest) = line.strip_prefix("func ") else {
        return;
    };
    // Receiver check: `func (r *T) Name(` → skip (method)
    if rest.starts_with('(') {
        return;
    }
    let Some((name_raw, params)) = rest.split_once('(') else {
        return;
    };
    let name = name_raw.trim();
    if name.is_empty() {
        return;
    }
    let params = &format!("({}", params);

    if name == "TestMain" {
        if params_match_type(params, "M") {
            result.test_main = Some(TestFunc {
                name: name.to_string(),
                package: pkg_label.to_string(),
            });
        } else if params_match_type(params, "T") {
            result.tests.push(TestFunc {
                name: name.to_string(),
                package: pkg_label.to_string(),
            });
        }
        return;
    }

    if is_test_name(name, "Test") && params_match_type(params, "T") {
        result.tests.push(TestFunc {
            name: name.to_string(),
            package: pkg_label.to_string(),
        });
    } else if is_test_name(name, "Benchmark") && params_match_type(params, "B") {
        result.benchmarks.push(TestFunc {
            name: name.to_string(),
            package: pkg_label.to_string(),
        });
    } else if is_test_name(name, "Fuzz") && params_match_type(params, "F") {
        result.fuzz_targets.push(TestFunc {
            name: name.to_string(),
            package: pkg_label.to_string(),
        });
    }
}

/// Returns true if `name` looks like a test/benchmark/fuzz function with the given prefix.
///
/// Mirrors Go's heuristic: the character after the prefix (if any) must not be a lowercase letter.
fn is_test_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    if rest.is_empty() {
        return true; // bare "Test", "Benchmark", "Fuzz" are valid
    }
    // First character after prefix must not be lowercase.
    !rest.starts_with(|c: char| c.is_lowercase())
}

/// Returns true if the parameter list contains `*testing.T`, `*testing.B`, etc. for `type_char`.
///
/// We accept either `*T` or `*testing.T` (the import alias can vary, but we only care about
/// the selector name matching `type_char`).
fn params_match_type(params: &str, type_char: &str) -> bool {
    // params looks like `(t *testing.T)` or `(t *T)` or `(m *testing.M)` etc.
    // We just scan for `*T` or `*testing.T` anywhere inside.
    params.contains(&format!("*testing.{}", type_char))
        || params.contains(&format!("*{}", type_char))
}

/// Analyse a set of test files (streamed line-by-line) and produce a combined [`Analysis`].
///
/// `files` is a slice of `(prefix, rel_name)` where `prefix` is `"_test"` (internal) or
/// `"_xtest"` (external) and `rel_name` is the source file name relative to `base_dir`.
/// `base_dir` is where the engine staged the inputs (e.g. `sandbox_pkg_dir`) — it is used
/// only to locate files on disk and is never recorded in the returned `Analysis`, so
/// caching downstream of this output remains hermetic.
pub fn analyze_test_main(
    import_path: &str,
    base_dir: &Path,
    files: &[(&str, &str)],
) -> anyhow::Result<Analysis> {
    let mut analysis = Analysis {
        import_path: import_path.to_string(),
        is_go1_18: true, // always assume modern Go
        ..Default::default()
    };

    for (prefix, name) in files {
        match *prefix {
            "_test" => analysis.import_test = true,
            "_xtest" => analysis.import_xtest = true,
            other => anyhow::bail!("unknown package prefix: {:?}", other),
        }

        let f = std::fs::File::open(base_dir.join(name))
            .with_context(|| format!("open {prefix} source {name}"))?;
        let partial = process_reader(prefix, BufReader::new(f))
            .with_context(|| format!("read {prefix} source {name}"))?;

        analysis.tests.extend(partial.tests);
        analysis.benchmarks.extend(partial.benchmarks);
        analysis.fuzz_targets.extend(partial.fuzz_targets);

        if let Some(tm) = partial.test_main {
            anyhow::ensure!(
                analysis.test_main.is_none(),
                "multiple definitions of TestMain in {name}"
            );
            analysis.test_main = Some(tm);
        }

        analysis.need_test = analysis.need_test || partial.need_test;
        analysis.need_xtest = analysis.need_xtest || partial.need_xtest;
    }

    Ok(analysis)
}

/// Generate the `testmain.go` source text from an [`Analysis`].
///
/// Output matches the template in the reference Go implementation.
pub fn generate_testmain(analysis: &Analysis) -> String {
    let mut out = String::new();

    out.push_str("// Code generated by Heph for test binary. DO NOT EDIT.\n");
    out.push_str("package main\n");
    out.push_str("import (\n");
    out.push_str("\t\"os\"\n");
    if analysis.test_main.is_some() {
        out.push_str("\t\"reflect\"\n");
    }
    out.push_str("\t\"testing\"\n");
    out.push_str("\t\"testing/internal/testdeps\"\n");
    if analysis.import_test {
        let alias = if analysis.need_test { "_test" } else { "_" };
        out.push_str(&format!("\t{} {:?}\n", alias, analysis.import_path));
    }
    if analysis.import_xtest {
        let alias = if analysis.need_xtest { "_xtest" } else { "_" };
        let xtest_path = format!("{}_test", analysis.import_path);
        out.push_str(&format!("\t{} {:?}\n", alias, xtest_path));
    }
    out.push_str(")\n");
    out.push('\n');

    // tests slice
    out.push_str("var tests = []testing.InternalTest{\n");
    for tf in &analysis.tests {
        out.push_str(&format!(
            "\t{{\"{}\" , {}.{}}},\n",
            tf.name, tf.package, tf.name
        ));
    }
    out.push_str("}\n");
    out.push('\n');

    // benchmarks slice
    out.push_str("var benchmarks = []testing.InternalBenchmark{\n");
    for tf in &analysis.benchmarks {
        out.push_str(&format!(
            "\t{{\"{}\" , {}.{}}},\n",
            tf.name, tf.package, tf.name
        ));
    }
    out.push_str("}\n");
    out.push('\n');

    // examples slice
    out.push_str("var examples = []testing.InternalExample{\n");
    for ex in &analysis.examples {
        out.push_str(&format!(
            "\t{{\"{}\" , {}.{}, {:?}, {}}},\n",
            ex.name, ex.package, ex.name, ex.output, ex.unordered
        ));
    }
    out.push_str("}\n");
    out.push('\n');

    // fuzz targets (Go ≥ 1.18)
    if analysis.is_go1_18 {
        out.push_str("var fuzzTargets = []testing.InternalFuzzTarget{\n");
        for tf in &analysis.fuzz_targets {
            out.push_str(&format!(
                "\t{{\"{}\" , {}.{}}},\n",
                tf.name, tf.package, tf.name
            ));
        }
        out.push_str("}\n");
        out.push('\n');
    }

    // init
    out.push_str("func init() {\n");
    out.push_str(&format!(
        "\ttestdeps.ImportPath = {:?}\n",
        analysis.import_path
    ));
    out.push_str("}\n");
    out.push('\n');

    // main
    out.push_str("func main() {\n");
    if analysis.is_go1_18 {
        out.push_str(
            "\tm := testing.MainStart(testdeps.TestDeps{}, tests, benchmarks, fuzzTargets, examples)\n",
        );
    } else {
        out.push_str(
            "\tm := testing.MainStart(testdeps.TestDeps{}, tests, benchmarks, examples)\n",
        );
    }
    if let Some(tm) = &analysis.test_main {
        out.push_str(&format!("\t{}.{}(m)\n", tm.package, tm.name));
        out.push_str("\tos.Exit(int(reflect.ValueOf(m).Elem().FieldByName(\"exitCode\").Int()))\n");
    } else {
        out.push_str("\tos.Exit(m.Run())\n");
    }
    out.push_str("}\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_source_finds_test() {
        let source = "package pkg\n\nfunc TestFoo(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert_eq!(partial.tests.len(), 1);
        assert_eq!(partial.tests[0].name, "TestFoo");
        assert_eq!(partial.tests[0].package, "_test");
    }

    #[test]
    fn test_block_commented_func_is_ignored() {
        // Regression: a `func Test…` at column 0 inside a `/* … */` block comment
        // must not be scanned as a test (it produced an undefined reference in
        // the generated testmain.go).
        let source = "package pkg\n\n\
            /* for manual testing only\n\
            func TestDiblManual(tt *testing.T) {\n\
            \ttt.Fatal(\"\")\n\
            }\n\
            */\n\n\
            func TestReal(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        let names: Vec<&str> = partial.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["TestReal"], "commented func must be skipped");
    }

    #[test]
    fn test_line_commented_func_is_ignored() {
        let source =
            "package pkg\n\n// func TestNope(t *testing.T) {}\nfunc TestYes(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        let names: Vec<&str> = partial.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["TestYes"]);
    }

    #[test]
    fn test_trailing_line_comment_keeps_func() {
        let source = "package pkg\n\nfunc TestFoo(t *testing.T) { // a comment\n}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert_eq!(partial.tests.len(), 1);
        assert_eq!(partial.tests[0].name, "TestFoo");
    }

    #[test]
    fn test_block_comment_open_in_string_does_not_swallow_funcs() {
        // A `/*` inside a string literal must NOT start a block comment that
        // would hide the real test below it.
        let source =
            "package pkg\n\nvar s = \"/* not a comment\"\nfunc TestKept(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        let names: Vec<&str> = partial.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["TestKept"]);
    }

    #[test]
    fn test_func_inside_raw_string_is_ignored() {
        // A backtick raw string spanning lines with `func Test…` inside is not code.
        let source = "package pkg\n\nvar tmpl = `\nfunc TestInRawString(t *testing.T) {}\n`\nfunc TestReal(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        let names: Vec<&str> = partial.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["TestReal"]);
    }

    #[test]
    fn test_process_source_ignores_testify_match() {
        // "Testify" starts with "Test" but next char is 'i' (lowercase) → skip
        let source = "package pkg\n\nfunc Testify(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert!(partial.tests.is_empty());
    }

    #[test]
    fn test_process_source_finds_benchmark() {
        let source = "package pkg\n\nfunc BenchmarkFoo(b *testing.B) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert_eq!(partial.benchmarks.len(), 1);
        assert_eq!(partial.benchmarks[0].name, "BenchmarkFoo");
    }

    #[test]
    fn test_process_source_finds_fuzz() {
        let source = "package pkg\n\nfunc FuzzFoo(f *testing.F) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert_eq!(partial.fuzz_targets.len(), 1);
        assert_eq!(partial.fuzz_targets[0].name, "FuzzFoo");
    }

    #[test]
    fn test_process_source_finds_test_main() {
        let source = "package pkg\n\nfunc TestMain(m *testing.M) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert!(partial.test_main.is_some());
        assert_eq!(partial.test_main.as_ref().unwrap().name, "TestMain");
    }

    #[test]
    fn test_process_source_method_ignored() {
        // Method with receiver should be skipped
        let source = "package pkg\n\nfunc (s *Suite) TestFoo(t *testing.T) {}\n";
        let partial = process_reader("_test", source.as_bytes()).unwrap();
        assert!(partial.tests.is_empty());
    }

    #[test]
    fn test_is_test_name_prefix_only() {
        assert!(is_test_name("Test", "Test"));
        assert!(is_test_name("Benchmark", "Benchmark"));
        assert!(is_test_name("Fuzz", "Fuzz"));
    }

    #[test]
    fn test_is_test_name_uppercase_suffix() {
        assert!(is_test_name("TestFoo", "Test"));
        assert!(is_test_name("TestHTTP", "Test"));
    }

    #[test]
    fn test_is_test_name_lowercase_suffix_rejected() {
        assert!(!is_test_name("Testify", "Test"));
        assert!(!is_test_name("Benchmarking", "Benchmark"));
    }

    #[test]
    fn test_analyze_test_main_empty() {
        let analysis = analyze_test_main("example.com/pkg", Path::new("/tmp"), &[]).unwrap();
        assert!(analysis.tests.is_empty());
        assert!(!analysis.import_test);
        assert!(!analysis.import_xtest);
    }

    #[test]
    fn test_analyze_test_main_streams_from_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let name = "pkg_test.go";
        std::fs::write(
            dir.path().join(name),
            "package pkg\n\nfunc TestAdd(t *testing.T) {}\n",
        )
        .unwrap();

        let files = vec![("_test", name)];
        let analysis = analyze_test_main("example.com/pkg", dir.path(), &files).unwrap();
        assert_eq!(analysis.tests.len(), 1);
        assert_eq!(analysis.tests[0].name, "TestAdd");
        assert!(analysis.import_test);
        assert!(analysis.need_test);
    }

    #[test]
    fn test_generate_testmain_basic() {
        let analysis = Analysis {
            import_path: "example.com/pkg".to_string(),
            import_test: true,
            need_test: true,
            is_go1_18: true,
            tests: vec![TestFunc {
                name: "TestFoo".to_string(),
                package: "_test".to_string(),
            }],
            ..Default::default()
        };
        let out = generate_testmain(&analysis);
        assert!(out.contains("package main"));
        assert!(out.contains("TestFoo"));
        assert!(out.contains("_test"));
        assert!(out.contains("\"example.com/pkg\""));
        assert!(out.contains("testdeps.ImportPath"));
        assert!(out.contains("os.Exit(m.Run())"));
        assert!(out.contains("fuzzTargets")); // go1.18
    }

    #[test]
    fn test_generate_testmain_with_test_main_func() {
        let analysis = Analysis {
            import_path: "example.com/pkg".to_string(),
            import_test: true,
            need_test: true,
            is_go1_18: true,
            test_main: Some(TestFunc {
                name: "TestMain".to_string(),
                package: "_test".to_string(),
            }),
            ..Default::default()
        };
        let out = generate_testmain(&analysis);
        assert!(out.contains("reflect"));
        assert!(out.contains("_test.TestMain(m)"));
        assert!(out.contains("exitCode"));
        assert!(!out.contains("os.Exit(m.Run())"));
    }

    #[test]
    fn test_generate_testmain_no_go118() {
        let analysis = Analysis {
            import_path: "example.com/pkg".to_string(),
            import_test: true,
            need_test: true,
            is_go1_18: false,
            ..Default::default()
        };
        let out = generate_testmain(&analysis);
        assert!(!out.contains("fuzzTargets"));
        assert!(
            out.contains("testing.MainStart(testdeps.TestDeps{}, tests, benchmarks, examples)")
        );
    }
}
